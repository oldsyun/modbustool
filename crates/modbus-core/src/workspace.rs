//! Serializable workspace: connections, poll definitions, register aliases.
//! Mirrors nmw's `.nmw` concept; we use `.mbw` (JSON) and version the schema.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ConnectionConfig {
    Tcp {
        name: String,
        host: String,
        port: u16,
        unit_id: u8,
        timeout_ms: u64,
        retries: u32,
    },
    Rtu {
        name: String,
        port: String,
        baud_rate: u32,
        data_bits: u8,
        stop_bits: u8,
        parity: String, // "none" | "odd" | "even"
        unit_id: u8,
        inter_frame_ms: u64,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PollConfig {
    pub name: String,
    pub connection: String, // references ConnectionConfig.name
    pub function: u8,       // 0x01..0x04
    pub start: u16,
    pub count: u16,
    pub interval_ms: u64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Workspace {
    pub schema: u32,
    pub connections: Vec<ConnectionConfig>,
    pub polls: Vec<PollConfig>,
    pub aliases: HashMap<String, String>,
}

impl Workspace {
    pub fn new() -> Self {
        Self {
            schema: 1,
            ..Default::default()
        }
    }

    /// Load a workspace from a `.mbw` JSON file.
    pub fn load(path: &Path) -> Result<Self, Box<dyn std::error::Error>> {
        let file = std::fs::File::open(path)?;
        let ws: Workspace = serde_json::from_reader(file)?;
        if ws.schema == 0 {
            return Err("unsupported workspace schema".into());
        }
        Ok(ws)
    }

    /// Persist the workspace to a `.mbw` JSON file (pretty-printed).
    pub fn save(&self, path: &Path) -> Result<(), Box<dyn std::error::Error>> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = std::fs::File::create(path)?;
        serde_json::to_writer_pretty(file, self)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn roundtrip_save_load() {
        let mut ws = Workspace::new();
        ws.connections.push(ConnectionConfig::Tcp {
            name: "plc1".into(),
            host: "127.0.0.1".into(),
            port: 1502,
            unit_id: 1,
            timeout_ms: 1000,
            retries: 3,
        });
        ws.polls.push(PollConfig {
            name: "temp".into(),
            connection: "plc1".into(),
            function: 0x03,
            start: 0,
            count: 10,
            interval_ms: 500,
        });
        ws.aliases.insert("40001".into(), "设定温度".into());

        let dir = std::env::temp_dir();
        let path = dir.join("mbw_test.mbw");
        ws.save(&path).unwrap();

        let loaded = Workspace::load(&path).unwrap();
        assert_eq!(loaded.schema, 1);
        assert_eq!(loaded.connections.len(), 1);
        assert_eq!(loaded.polls[0].count, 10);
        assert_eq!(loaded.aliases.get("40001").unwrap(), "设定温度");

        // cleanup
        let _ = std::fs::remove_file(&path);
        let _ = Write::write_all(&mut std::io::sink(), b"");
    }
}
