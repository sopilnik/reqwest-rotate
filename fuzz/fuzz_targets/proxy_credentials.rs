#![no_main]

use libfuzzer_sys::fuzz_target;
use reqwest_rotate::{ProxyList, RotatingClient};

const SECRET: &str = "pw-7f3a9c1e";

// The password in a proxy URL must not show up in any text the crate
// produces about an entry: the list, the builder that holds the entry as
// written, or the error for a rejected one. The input is split at NUL bytes
// into the text before `user:` (a scheme, slashes, whitespace), the text
// before the secret and after it inside the password, and the host and
// beyond. So a password such as `1234/<secret>`, a no-break space ahead of
// the scheme or `http:///` are reachable as well as whatever follows the
// `@`. The dictionary in `proxy_credentials.dict` seeds the common pieces.
fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    let mut parts = text.splitn(4, '\0');
    let lead = parts.next().unwrap_or_default();
    let before = parts.next().unwrap_or_default();
    let after = parts.next().unwrap_or_default();
    let tail = parts.next().unwrap_or_default();
    // An `@` ahead of the secret can end the userinfo there and leave the
    // secret as the host or in the path, which is not a password.
    if lead.contains('@')
        || before.contains('@')
        || [lead, before, after, tail]
            .iter()
            .any(|part| part.contains(SECRET))
        || host_spells_secret(tail)
    {
        return;
    }
    if shows_secret(&format!("{lead}{tail}")) {
        return;
    }
    let entry = format!("{lead}user:{before}{SECRET}{after}@{tail}");

    assert!(!shows_secret(&entry), "credentials shown for {entry:?}");
});

// Whether the host side, as the URL parser reads it (tabs and newlines
// dropped, lowercased), spells the secret, which is not a leak.
fn host_spells_secret(tail: &str) -> bool {
    let read: String = tail
        .chars()
        .filter(|c| !matches!(c, '\t' | '\r' | '\n'))
        .collect();

    read.to_lowercase().contains(SECRET)
}

// Whether any text the crate gives for this one entry holds the secret. Run
// on the entry without credentials first, this also tells when the entry
// shows what it holds anyway, which is not a leak of the password.
fn shows_secret(entry: &str) -> bool {
    texts_about(entry).iter().any(|text| text.contains(SECRET))
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
