use std::collections::HashMap;
use std::sync::RwLock;

use chrono::{DateTime, Utc};

#[derive(Clone, Debug)]
pub struct GpsCoord {
    pub latitude:  f64,
    pub longitude: f64,
}

#[derive(Clone, Debug)]
pub struct LocationEntry {
    pub coord:      GpsCoord,
    pub accuracy:   Option<f64>,
    pub updated_at: DateTime<Utc>,
    pub is_live:    bool,
}

pub struct LocationManager {
    locations: RwLock<HashMap<String, LocationEntry>>,
}

impl LocationManager {
    pub fn new() -> Self {
        Self { locations: RwLock::new(HashMap::new()) }
    }

    pub fn update(&self, source: &str, coord: GpsCoord, accuracy: Option<f64>, is_live: bool) {
        let entry = LocationEntry { coord, accuracy, updated_at: Utc::now(), is_live };
        self.locations.write().unwrap().insert(source.to_string(), entry);
    }

    pub fn get(&self, source: &str) -> Option<LocationEntry> {
        self.locations.read().unwrap().get(source).cloned()
    }

    pub fn latest(&self) -> Option<(String, LocationEntry)> {
        self.locations.read().unwrap()
            .iter()
            .max_by_key(|(_, e)| e.updated_at)
            .map(|(k, v)| (k.clone(), v.clone()))
    }

    pub fn all(&self) -> Vec<(String, LocationEntry)> {
        self.locations.read().unwrap()
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }
}

impl Default for LocationManager {
    fn default() -> Self { Self::new() }
}

/// Write-only view of a location store.
///
/// Plugins store `Arc<dyn LocationUpdater>` so they can report GPS fixes
/// without depending on the concrete `LocationManager` struct.
pub trait LocationUpdater: Send + Sync {
    fn update(&self, source: &str, coord: GpsCoord, accuracy: Option<f64>, is_live: bool);
}

impl LocationUpdater for LocationManager {
    fn update(&self, source: &str, coord: GpsCoord, accuracy: Option<f64>, is_live: bool) {
        LocationManager::update(self, source, coord, accuracy, is_live);
    }
}
