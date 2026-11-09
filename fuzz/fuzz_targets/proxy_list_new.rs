#![no_main]

use std::time::Duration;

use libfuzzer_sys::fuzz_target;
use reqwest_rotate::ProxyList;

const COOLDOWN: Duration = Duration::from_secs(60);

// One entry per line, the way a proxy list usually comes from a file. Any
// list `new` accepts must come back in a form it accepts again unchanged,
// and every entry must be found by its original spelling.
fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    let entries: Vec<&str> = text.split('\n').collect();
    let Ok(list) = ProxyList::new(&entries) else {
        return;
    };

    let canonical: Vec<&str> = list.iter().collect();
    assert!(!canonical.is_empty());
    assert!(canonical.len() <= entries.len());
    assert_eq!(canonical.len(), list.len());

    let again = ProxyList::new(&canonical).expect("canonical URLs are accepted again");
    assert_eq!(again.iter().collect::<Vec<_>>(), canonical);

    for entry in &entries {
        assert!(list.mark_bad(entry, COOLDOWN));
        assert!(list.in_cooldown(entry));
        assert!(list.mark_good(entry));
        assert!(!list.in_cooldown(entry));
    }
});
