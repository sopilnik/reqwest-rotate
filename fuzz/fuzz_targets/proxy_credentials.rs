#![no_main]

use libfuzzer_sys::fuzz_target;
use reqwest_rotate::{ProxyList, RotatingClient};

const SECRET: &str = "pw-7f3a9c1e";

// The secret must not show up in any text the crate produces about an
// entry: the list, the builder that holds the entry as written, or the
// error for a rejected one. It is put into the entry wherever the input has
// its first NUL byte, so it can sit in a password (`1234/<secret>`), after
// a scheme typo (`http:/1.2.3.4:8080:user:<secret>`), in the reversed vendor
// form (`host:80@alice:123#<secret>`), in a path, a query or a fragment,
// ahead of the scheme, or after extra slashes. Only the host and the scheme
// may spell it. The dictionary in `proxy_credentials.dict` seeds the common
// pieces.
fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    if text.contains(SECRET) {
        return;
    }
    let (head, rest) = text.split_once('\0').unwrap_or((text, ""));
    let entry = format!("{head}{SECRET}{rest}");
    if host_or_scheme_spells_secret(&entry) {
        return;
    }

    assert!(
        texts_about(&entry)
            .iter()
            .all(|text| !text.contains(SECRET)),
        "the secret is shown for {entry:?}"
    );
});

// Whether the host or the scheme, as the URL parser reads the entry (tabs
// and newlines dropped, the ends trimmed, lowercased, the authority ending
// at the first `/`, `?`, `#` or `\`, its host after the last `@`), spells the
// secret. That is the entry's own text and not a leak.
fn host_or_scheme_spells_secret(entry: &str) -> bool {
    let read: String = entry
        .chars()
        .filter(|c| !matches!(c, '\t' | '\r' | '\n'))
        .collect::<String>()
        .trim()
        .trim_matches(|c: char| c <= ' ')
        .to_lowercase();
    // The crate gives an entry without a `://` an `http://` first.
    let parsed = if read.contains("://") {
        read.clone()
    } else {
        format!("http://{read}")
    };
    let (scheme, after_scheme) = parsed.split_once(':').unwrap_or_default();
    let authority_start = after_scheme.trim_start_matches(['/', '\\']);
    let authority_end = authority_start
        .find(['/', '?', '#', '\\'])
        .unwrap_or(authority_start.len());
    let authority = &authority_start[..authority_end];
    let host = authority.rsplit('@').next().unwrap_or_default();
    let host = host.split(':').next().unwrap_or_default();
    // An entry the parser refuses is shown from its last `@` on, which is
    // then the host as far as the crate can tell.
    let last = read.rsplit('@').next().unwrap_or_default();
    let last = last
        .split([':', '/', '?', '#', '\\'])
        .next()
        .unwrap_or_default();

    scheme.contains(SECRET) || host.contains(SECRET) || last.contains(SECRET)
}

// The builder's `Debug` for every entry, since it holds the entry as
// written whether or not `ProxyList::new` takes it; then the list's `Debug`
// and redacted URLs when it is accepted, the error and its causes when it
// is not.
fn texts_about(entry: &str) -> Vec<String> {
    let mut texts = vec![format!("{:?}", RotatingClient::builder().proxies([entry]))];
    match ProxyList::new([entry]) {
        Ok(list) => {
            texts.push(format!("{list:?}"));
            texts.extend(list.iter_redacted());
        }
        Err(error) => {
            texts.push(format!("{error:?}"));
            let mut cause: Option<&dyn std::error::Error> = Some(&error);
            while let Some(current) = cause {
                texts.push(current.to_string());
                cause = current.source();
            }
        }
    }

    texts
}
