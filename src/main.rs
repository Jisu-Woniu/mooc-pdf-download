use std::{
    convert::AsRef,
    path::Path,
    sync::{Arc, LazyLock},
    time::{Duration, SystemTime},
};

use bytes::Bytes;
use dialoguer::Input;
use eyre::OptionExt as _;
use futures::future::join_all;
use indexmap::indexmap;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use regex::bytes::Regex;
use reqwest::{
    header::{HeaderMap, HeaderValue},
    Client, Url,
};
use rookie::{chrome, chromium, edge, enums::CookieToString as _, firefox};
use tokio::{
    fs::{create_dir_all, File},
    io::{AsyncWriteExt, BufWriter},
    spawn,
    sync::mpsc,
};

use crate::{cookies::CookieJar, query_string::unquote_plus};

mod cookies;
mod query_string;

fn headers() -> HeaderMap {
    let mut header = HeaderMap::new();

    header.insert("content-type", HeaderValue::from_static("text/plain"));

    header
}

async fn get_course_info(client: &Client, session_id: &str, tid: &str) -> eyre::Result<Bytes> {
    let form = indexmap! {
        "callCount" => "1".to_string(),
        "scriptSessionId" => "${scriptSessionId}190".to_string(),
        "httpSessionId" => session_id.to_string(),
        "c0-scriptName" => "CourseBean".to_string(),
        "c0-methodName" => "getLastLearnedMocTermDto".to_string(),
        "c0-id" => "0".to_string(),
        "c0-param0" => format!("number:{}", tid),
        "batchId" => SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)?
            .as_millis()
            .to_string(),
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

fn get_content_ids(course_info: &Bytes) -> Vec<String> {
    static REGEX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"contentId=([0-9]+);").unwrap());
    REGEX
        .captures_iter(course_info)
        .map(|cap| String::from_utf8(cap[1].to_vec()).unwrap())
        .collect()
}

fn get_ids(course_info: &Bytes) -> Vec<String> {
    static REGEX: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"id=([0-9]+);s[0-9]+\.jsonContent").unwrap());
    REGEX
        .captures_iter(course_info)
        .map(|cap| String::from_utf8(cap[1].to_vec()).unwrap())
        .collect()
}

async fn get_pdf_urls<S: AsRef<str>>(
    client: &Client,
    session_id: &str,
    content_ids: &[S],
    section_ids: &[S],
) -> eyre::Result<Vec<String>> {
    static REGEX: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r#"textOrigUrl:"([^"]*\.pdf[^"]*)""#).unwrap());
    let (tx, mut rx) = mpsc::channel(5);
    for (content_id, section_id) in content_ids.iter().zip(section_ids) {
        let form = indexmap! {
            "callCount" => "1".to_string(),
            "scriptSessionId" => "${scriptSessionId}190".to_string(),
            "httpSessionId" => session_id.to_string(),
            "c0-scriptName" => "CourseBean".to_string(),
            "c0-methodName" => "getLessonUnitLearnVo".to_string(),
            "c0-id" => "0".to_string(),
            "c0-param0" => format!("number:{}", content_id.as_ref()),
            "c0-param1" => "number:3".to_string(),
            "c0-param2" => "number:0".to_string(),
            "c0-param3" => format!("number:{}", section_id.as_ref()),
            "batchId" => SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)?
                .as_millis()
                .to_string(),
        };

        let client = client.clone();
        let tx = tx.clone();

        spawn(async move {
            let s = client
                    .post(
                        "https://www.icourse163.org/dwr/call/plaincall/CourseBean.getLessonUnitLearnVo.dwr",
                    )
                    .form(&form)
                    .send()
                    .await?
                    .error_for_status()?
                    .bytes()
                    .await?;

            if let Some(url) = REGEX
                .captures(&s)
                .map(|cap| String::from_utf8_lossy(&cap[1]).into_owned())
            {
                tx.send(url).await?;
            }
            eyre::Ok(())
        });
    }
    drop(tx);
    let mut urls = Vec::new();

    while let Some(url) = rx.recv().await {
        urls.push(url);
    }
    Ok(urls)
}

async fn download<P: AsRef<Path>, S: AsRef<str> + Sync>(
    client: &Client,
    urls: &[S],
    path: P,
    multi_progress: &MultiProgress,
) -> eyre::Result<()> {
    let path = path.as_ref();
    create_dir_all(&path).await?;
    // Make sure all the URLs are downloaded concurrently until completion or error
    let x = join_all(
        urls.iter()
            .map(move |url| {
                let url = url.as_ref().to_string();
                let client = client.clone();
                let multi_progress = multi_progress.clone();
                static REGEX: LazyLock<Regex> =
                    LazyLock::new(|| Regex::new(r"[?&]download=([^&]*)").unwrap());
                let file_name = REGEX
                    .captures(url.as_bytes())
                    .and_then(|capture| unquote_plus(&capture[1]).ok())
                    .expect("No filename found in URL");
                let path = path.join(&file_name);
                tokio::spawn(async move {
                    let mut response = client.get(&url).send().await?.error_for_status()?;

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
                })
            })
            .collect::<Vec<_>>(),
    )
    .await;

    for res in x {
        res??;
    }

    Ok(())
}

fn set_cookies(cookie_source: &str, domain: &Url) -> eyre::Result<CookieJar> {
    let cookie_string = match cookie_source {
        "chrome" => chrome(Some(vec!["icourse163.org".to_string()]))?.to_string(),
        "edge" => edge(Some(vec!["icourse163.org".to_string()]))?.to_string(),
        "chromium" => chromium(Some(vec!["icourse163.org".to_string()]))?.to_string(),
        "firefox" => firefox(Some(vec!["icourse163.org".to_string()]))?.to_string(),
        // Safari is only available on macOS
        #[cfg(target_os = "macos")]
        "safari" => rookie::safari(Some(vec!["icourse163.org".to_string()]))?.to_string(),
        _ => cookie_source.to_string(),
    };

    let cookie_jar = CookieJar::default();

    cookie_jar.add_cookie_str(&cookie_string, domain);

    Ok(cookie_jar)
}

#[tokio::main]
async fn main() -> eyre::Result<()> {
    let tid: String = Input::new()
        .with_prompt("Enter the tid of course")
        .interact_text()?;

    let cookie_source: String = Input::new()
        .with_prompt("Enter the cookies")
        .interact_text()?;
    let domain = Url::parse("https://www.icourse163.org").unwrap();

    let cookie_store = Arc::new(set_cookies(&cookie_source, &domain)?);

    let session_id = cookie_store
        .get_session_id(&domain)
        .ok_or_eyre("Session ID (NTESSTUDYSI) not found in cookie")?;

    let client = Client::builder()
        .cookie_provider(cookie_store)
        .user_agent("Mozilla/5.0 (X11; Linux x86_64; rv:130.0) Gecko/20100101 Firefox/130.0")
        .build()?;

    let multi_progress = MultiProgress::new();

    let spinner =
        multi_progress.add(ProgressBar::new_spinner().with_message("Fetching course info"));
    spinner.enable_steady_tick(Duration::from_millis(100));
    let course_info = get_course_info(&client, &session_id, &tid).await?;

    spinner.set_message("Analyzing course info");
    let content_ids = get_content_ids(&course_info);
    let section_ids = get_ids(&course_info);
    spinner.finish_with_message("Fetching course info done");

    let spinner = multi_progress.add(ProgressBar::new_spinner().with_message("Fetching PDF URLs"));
    spinner.enable_steady_tick(Duration::from_millis(100));
    let urls = get_pdf_urls(&client, &session_id, &content_ids, &section_ids).await?;
    spinner.finish_with_message("Fetching PDF URLs done");

    download(
        &client,
        &urls,
        Path::new("download").join(tid),
        &multi_progress,
    )
    .await?;

    Ok(())
}
