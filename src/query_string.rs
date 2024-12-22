use std::string::FromUtf8Error;

/// Replaces %xx escapes by their single-character equivalent.
///
/// # Examples
///
/// ```
/// use urlparse::unquote;
///
/// let s = unquote("ABC%3D123%21%20DEF%3D%23%23");
/// assert_eq!(s.ok().unwrap(), "ABC=123! DEF=##");
/// ```
///
pub fn unquote<S: AsRef<[u8]>>(s: S) -> Result<String, FromUtf8Error> {
    let mut result: Vec<u8> = Vec::new();
    let mut items = s.as_ref().split(|&b| b == b'%');
    match items.next() {
        Some(item) => result.append(&mut item.to_vec()),
        None => return String::from_utf8(result),
    }
    for item in items {
        match item.len() {
            0 => result.push(b'%'),
            1 => {
                result.push(b'%');
                result.append(&mut item.to_vec());
            }
            _ => {
                let fs = &item[..2];
                let ls = &item[2..];
                if let Some(digit) = to_digit(*fs.first().unwrap(), *fs.get(1).unwrap()) {
                    result.push(digit);
                    result.append(&mut ls.to_vec());
                } else {
                    result.push(b'%');
                    result.append(&mut item.to_vec());
                }
            }
        }
    }
    String::from_utf8(result)
}

/// Like unquote(), but also replaces plus signs by spaces, as required for
/// unquoting HTML form values.
///
/// # Examples
///
/// ```
/// use urlparse::unquote_plus;
///
/// let s = unquote_plus("ABC%3D123%21+DEF%3D%23%23");
/// assert_eq!(s.ok().unwrap(), "ABC=123! DEF=##");
/// ```
///
pub fn unquote_plus<S: AsRef<[u8]>>(s: S) -> Result<String, FromUtf8Error> {
    let s: Vec<u8> = s
        .as_ref()
        .iter()
        .map(|&b| match b {
            b'+' => b' ',
            _ => b,
        })
        .collect();
    unquote(s)
}

fn to_digit(n1: u8, n2: u8) -> Option<u8> {
    const DIGIT: &[u8] = b"0123456789ABCDEF";
    let n1 = n1.to_ascii_uppercase();
    let n2 = n2.to_ascii_uppercase();

    let mut byte: u8 = 0;
    let n1 = DIGIT.binary_search(&n1).ok()?;
    byte += n1 as u8 * 16;

    let n2 = DIGIT.binary_search(&n2).ok()?;
    byte += n2 as u8;

    Some(byte)
}
