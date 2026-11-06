use std::collections::{HashMap, HashSet};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

static READERS: LazyLock<Mutex<HashMap<String, (String, Instant)>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

pub(crate) fn update(recording_id: String, lease: String, in_use: bool) {
    let mut readers = READERS.lock().unwrap_or_else(|error| error.into_inner());
    if in_use {
        readers.insert(lease, (recording_id, Instant::now()));
    } else if readers
        .get(&lease)
        .is_some_and(|(id, _)| id == &recording_id)
    {
        readers.remove(&lease);
    }
}

pub(crate) fn active_recordings() -> HashSet<String> {
    let mut readers = READERS.lock().unwrap_or_else(|error| error.into_inner());
    readers.retain(|_, (_, time)| time.elapsed() < Duration::from_secs(60));
    readers.values().map(|(id, _)| id.clone()).collect()
}
