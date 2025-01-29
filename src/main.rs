use std::{
    borrow::Cow,
    convert::{AsRef, Infallible},
    fmt::{Display, Formatter},
    path::Path,
    str::FromStr,
    sync::{Arc, LazyLock},
    time::{Duration, SystemTime},
};

use bytes::Bytes;
use dialoguer::{Input, Select};
use eyre::OptionExt as _;
use indexmap::indexmap;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use memchr::{memchr, memmem::find_iter};
use rand::{rng, seq::IndexedRandom as _};
use regex::bytes::Regex;
use reqwest::{
    header::{HeaderMap, HeaderValue},
    Client, Url,
};
use rookie::{chrome, chromium, edge, enums::CookieToString as _, firefox, opera};
use tokio::{
    fs::{create_dir_all, File},
    io::{AsyncWriteExt as _, BufWriter},
    spawn,
    sync::mpsc,
    task::JoinSet,
};

use crate::{cookies::CookieJar, query_string::unquote_plus, user_agents::USER_AGENTS};

mod cookies;
mod query_string;
mod user_agents;

fn headers() -> HeaderMap {
    let mut header = HeaderMap::new();
    header.insert("content-type", HeaderValue::from_static("text/plain"));
    header
}

async fn get_course_info(client: &Client, session_id: &str, tid: &str) -> eyre::Result<Bytes> {
    let form = indexmap! {
        "callCount" => Cow::from("1"),
        "scriptSessionId" => Cow::from("${scriptSessionId}190"),
        "httpSessionId" => Cow::from(session_id),
        "c0-scriptName" => Cow::from("CourseBean"),
        "c0-methodName" => Cow::from("getLastLearnedMocTermDto"),
        "c0-id" => Cow::from("0"),
        "c0-param0" => Cow::from(format!("number:{}", tid)),
        "batchId" => Cow::from(
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)?
                .as_millis()
                .to_string(),
        ),
    };

    let bytes = client
        .post(
            "https://www.icourse163.org/dwr/call/plaincall/CourseBean.getLastLearnedMocTermDto.dwr",
        )
        .headers(headers())
        .form(&form)
        .send()
        .await?
        .error_for_status()?
        .bytes()
        .await?;

    Ok(bytes)
}

fn get_ids(course_info: &Bytes) -> Vec<(String, String)> {
    static REGEX: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"s([0-9]+)\.contentId=([0-9]+);").unwrap());
    REGEX
        .captures_iter(course_info)
        .map(|cap| {
            let (_, [ident, content_id]) = cap.extract();
            let n = String::from_utf8_lossy(ident);
            let content_id = String::from_utf8_lossy(content_id);

            let section_id = {
                let section_id_pattern = format!("s{n}.id=");

                let pos = find_iter(course_info, &section_id_pattern)
                    .next()
                    .expect("No section ID found")
                    + section_id_pattern.len();

                let haystack = &course_info[pos..];
                let offset = memchr(b';', haystack).unwrap_or(haystack.len());
                String::from_utf8_lossy(&course_info[pos..pos + offset])
            };

            (content_id.into_owned(), section_id.into_owned())
        })
        .collect()
}

async fn get_pdf_urls<S: AsRef<str>>(
    client: &Client,
    session_id: &str,
    ids: &[(S, S)],
) -> eyre::Result<Vec<Url>> {
    let (tx, mut rx) = mpsc::channel(5);
    for (content_id, section_id) in ids {
        let form = indexmap! {
            "callCount" => Cow::from("1"),
            "scriptSessionId" => Cow::from("${scriptSessionId}190"),
            "httpSessionId" => Cow::from(session_id),
            "c0-scriptName" => Cow::from("CourseBean"),
            "c0-methodName" => Cow::from("getLessonUnitLearnVo"),
            "c0-id" => Cow::from("0"),
            "c0-param0" => Cow::from(format!("number:{}", content_id.as_ref())),
            "c0-param1" => Cow::from("number:3"),
            "c0-param2" => Cow::from("number:0"),
            "c0-param3" => Cow::from(format!("number:{}", section_id.as_ref())),
            "batchId" => Cow::from(
                SystemTime::now()
                    .duration_since(SystemTime::UNIX_EPOCH)?
                    .as_millis()
                    .to_string(),
            ),
        };

        let client = client.clone();
        let tx = tx.clone();

        let request = client
            .post(
                "https://www.icourse163.org/dwr/call/plaincall/CourseBean.getLessonUnitLearnVo.dwr",
            )
            .form(&form);

        spawn(async move {
            let s = request.send().await?.error_for_status()?.bytes().await?;

            static REGEX: LazyLock<Regex> =
                LazyLock::new(|| Regex::new(r#"textOrigUrl:"([^"]*\.pdf[^"]*)""#).unwrap());

            if let Some(url) = REGEX
                .captures(&s)
                .map(|cap| String::from_utf8_lossy(&cap[1]).into_owned())
            {
                tx.send(url).await?;
            }
            eyre::Ok(())
        });
    }

    // There is still one instance of `tx`, and we need to drop it to close the channel.
    drop(tx);

    let mut urls = Vec::new();

    while let Some(url) = rx.recv().await {
        urls.push(Url::parse(&url)?);
    }
    Ok(urls)
}

async fn download<P: AsRef<Path>>(
    client: &Client,
    urls: impl IntoIterator<Item = Url>,
    path: P,
    multi_progress: &MultiProgress,
) -> eyre::Result<()> {
    let path = path.as_ref();
    create_dir_all(&path).await?;
    let mut join_set = JoinSet::new();
    // Make sure all the URLs are downloaded concurrently until completion or error
    for url in urls {
        let client = client.clone();
        let multi_progress = multi_progress.clone();
        let file_name = url
            .query_pairs()
            .find(|(k, _)| matches!(k.as_ref(), "download"))
            .and_then(|(_, v)| unquote_plus(v.as_bytes()).ok())
            .ok_or_eyre("No filename found in URL")?;
        let path = path.join(&file_name);

        join_set.spawn(async move {
            let mut response = client.get(url).send().await?.error_for_status()?;

            let mut file = BufWriter::new(File::create(path).await?);

            let pb = response.content_length().map(|len| {
                multi_progress.add(
                    ProgressBar::new(len).with_prefix(file_name).with_style(
                        ProgressStyle::with_template(
                            "{prefix} {wide_bar} {binary_bytes}/{binary_total_bytes}",
                        )
                        .unwrap(),
                    ),
                )
            });

            while let Some(chunk) = response.chunk().await? {
                if let Some(pb) = &pb {
                    pb.inc(chunk.len() as u64);
                }
                file.write_all(&chunk).await?;
            }

            eyre::Ok(())
        });
    }

    let mut errors = Vec::new();

    while let Some(res) = join_set.join_next().await {
        match res {
            Ok(Err(e)) => errors.push(e),
            Err(e) => errors.push(e.into()),
            _ => {}
        }
    }

    Ok(())
}

fn set_cookies(cookie_source: CookieSource, domain: &Url) -> eyre::Result<CookieJar> {
    let cookie_string = match cookie_source {
        CookieSource::Chrome => chrome(Some(vec!["icourse163.org".to_string()]))?.to_string(),
        CookieSource::Edge => edge(Some(vec!["icourse163.org".to_string()]))?.to_string(),
        CookieSource::Chromium => chromium(Some(vec!["icourse163.org".to_string()]))?.to_string(),
        CookieSource::Firefox => firefox(Some(vec!["icourse163.org".to_string()]))?.to_string(),
        CookieSource::Opera => opera(Some(vec!["icourse163.org".to_string()]))?.to_string(),
        #[cfg(target_os = "macos")]
        CookieSource::Safari => {
            rookie::safari(Some(vec!["icourse163.org".to_string()]))?.to_string()
        }
        CookieSource::Custom(s) => s,
    };

    let cookie_jar = CookieJar::default();
    cookie_jar.add_cookie_str(&cookie_string, domain);

    Ok(cookie_jar)
}

#[derive(Debug, Clone)]
enum CookieSource {
    Chrome,
    Edge,
    Chromium,
    Firefox,
    Opera,
    #[cfg(target_os = "macos")]
    Safari,
    Custom(String),
}

impl Display for CookieSource {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.write_fmt(format_args!("{:?}", self))
    }
}

impl FromStr for CookieSource {
    type Err = Infallible;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "Chrome" => Ok(Self::Chrome),
            "Edge" => Ok(Self::Edge),
            "Chromium" => Ok(Self::Chromium),
            "Firefox" => Ok(Self::Firefox),
            "Opera" => Ok(Self::Opera),
            #[cfg(target_os = "macos")]
            "Safari" => Ok(Self::Safari),
            _ => Ok(Self::Custom(s.to_string())),
        }
    }
}

fn select_cookie_source() -> eyre::Result<CookieSource> {
    const COOKIE_SOURCES_TEXT: &[&str] = &[
        "Chrome",
        "Edge",
        "Chromium",
        "Firefox",
        "Opera",
        #[cfg(target_os = "macos")]
        "Safari",
        "Custom",
    ];
    let cookie_source_selection = Select::new()
        .with_prompt("Select the browser to use its cookies, or Custom to enter your own")
        .items(COOKIE_SOURCES_TEXT)
        .interact()?;

    let mut cookie_source = COOKIE_SOURCES_TEXT[cookie_source_selection].parse()?;

    if let CookieSource::Custom(..) = cookie_source {
        cookie_source = CookieSource::Custom(
            Input::new()
                .with_prompt("Enter the cookies")
                .interact_text()?,
        );
    }
    Ok(cookie_source)
}

#[tokio::main]
async fn main() -> eyre::Result<()> {
    let tid = Input::<'_, String>::new()
        .with_prompt("Enter the tid of course")
        .interact_text()?;

    let cookie_source = select_cookie_source()?;

    let domain = Url::parse("https://www.icourse163.org").unwrap();

    let cookie_store = Arc::new(set_cookies(cookie_source, &domain)?);

    let session_id = cookie_store
        .get_session_id(&domain)
        .ok_or_eyre("Session ID (NTESSTUDYSI) not found in cookie")?;

    let client = Client::builder()
        .cookie_provider(cookie_store)
        .user_agent(*USER_AGENTS.choose(&mut rng()).unwrap())
        .build()?;

    let multi_progress = MultiProgress::new();

    let spinner =
        multi_progress.add(ProgressBar::new_spinner().with_message("Fetching course info"));
    spinner.enable_steady_tick(Duration::from_millis(100));
    let course_info = get_course_info(&client, &session_id, &tid).await?;
    spinner.set_message("Analyzing course info");
    let ids = get_ids(&course_info);
    spinner.finish_with_message("Fetching course info done");

    let spinner = multi_progress.add(ProgressBar::new_spinner().with_message("Fetching PDF URLs"));
    spinner.enable_steady_tick(Duration::from_millis(100));
    let urls = get_pdf_urls(&client, &session_id, &ids).await?;
    spinner.finish_with_message("Fetching PDF URLs done");

    download(
        &client,
        urls,
        Path::new("download").join(tid),
        &multi_progress,
    )
    .await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use reqwest::Url;

    #[test]
    fn test() {
        dbg!(Url::parse("https://duckduckgo.com/?t=ffab&q=url+parts&ia=web").unwrap());
    }
}
