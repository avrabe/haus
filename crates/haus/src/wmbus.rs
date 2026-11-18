//! The wireless M-Bus source (REQ-WMBUS-001, DD-025): haus follows the
//! JSON lines wmbusmeters writes on the deployment host and keeps the
//! owner's heating cost allocators only.
//!
//! HAUS_WMBUS_LOG names the log file, HAUS_WMBUS_METERS the private meter
//! list (both or neither). At start the whole file is read (a value already
//! stored is stored again unchanged), then new lines every 10 s. A file that
//! shrinks or is replaced is read again from its start.

use crate::state::{run_blocking, AppState};
use haus_core::wmbus::{parse_line, Meters};
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::time::Duration;

/// The source's settings, checked at startup.
#[derive(Clone, Debug)]
pub struct WmbusConfig {
    pub log: PathBuf,
    pub meters: Meters,
}

/// Read and check HAUS_WMBUS_LOG and HAUS_WMBUS_METERS; None when neither
/// is set, an error naming the setting when only one is or the list is bad.
pub fn config(log: Option<&str>, meters: Option<&str>) -> Result<Option<WmbusConfig>, String> {
    let set = |v: Option<&str>| v.filter(|v| !v.trim().is_empty()).map(str::to_string);
    match (set(log), set(meters)) {
        (None, None) => Ok(None),
        (Some(_), None) => {
            Err("HAUS_WMBUS_LOG needs HAUS_WMBUS_METERS (the owner's meters)".into())
        }
        (None, Some(_)) => {
            Err("HAUS_WMBUS_METERS needs HAUS_WMBUS_LOG (the wmbusmeters log)".into())
        }
        (Some(log), Some(path)) => {
            let json = std::fs::read_to_string(&path)
                .map_err(|e| format!("HAUS_WMBUS_METERS {path}: {e}"))?;
            let meters =
                Meters::parse(&json).map_err(|e| format!("HAUS_WMBUS_METERS {path}: {e}"))?;
            Ok(Some(WmbusConfig {
                log: PathBuf::from(log),
                meters,
            }))
        }
    }
}

/// Where reading stopped: the file (to notice a replaced one), the offset
/// and an incomplete last line.
#[derive(Default)]
struct Cursor {
    #[cfg(unix)]
    inode: u64,
    offset: u64,
    partial: String,
}

/// The complete new lines of the log since the cursor.
fn read_new(path: &std::path::Path, c: &mut Cursor) -> std::io::Result<Vec<String>> {
    let mut f = std::fs::File::open(path)?;
    let meta = f.metadata()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if meta.ino() != c.inode {
            *c = Cursor {
                inode: meta.ino(),
                ..Cursor::default()
            };
        }
    }
    if meta.len() < c.offset {
        // Truncated: start over.
        c.offset = 0;
        c.partial.clear();
    }
    f.seek(SeekFrom::Start(c.offset))?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf)?;
    c.offset += buf.len() as u64;
    let text = String::from_utf8_lossy(&buf);
    let mut all = std::mem::take(&mut c.partial);
    all.push_str(&text);
    let mut lines: Vec<String> = all.split('\n').map(str::to_string).collect();
    c.partial = lines.pop().unwrap_or_default();
    Ok(lines)
}

/// Follow the log for as long as haus runs.
pub async fn run(state: AppState, cfg: WmbusConfig) {
    let sources = state.sources.clone();
    let Some(health) = sources.wmbus.as_ref() else {
        return;
    };
    tracing::info!(
        "wM-Bus: following {} for {} meter(s)",
        cfg.log.display(),
        cfg.meters.len()
    );
    let mut cursor = Cursor::default();
    let mut tick = tokio::time::interval(Duration::from_secs(10));
    loop {
        tick.tick().await;
        let log = cfg.log.clone();
        let meters = cfg.meters.clone();
        let ingest = state.ingest.clone();
        let mut c = std::mem::take(&mut cursor);
        let r = run_blocking(move || {
            let lines = read_new(&log, &mut c);
            let out = lines.map(|lines| {
                let items: Vec<_> = lines.iter().flat_map(|l| parse_line(l, &meters)).collect();
                let n = items.len();
                let stored = if items.is_empty() {
                    Ok(())
                } else {
                    ingest.ingest(&items).map(|_| ())
                };
                (n, stored)
            });
            (c, out)
        })
        .await;
        match r {
            (c, Ok((n, stored))) => {
                cursor = c;
                health.set_connected(true);
                if n > 0 {
                    health.record_message();
                }
                if let Err(e) = stored {
                    tracing::warn!("wM-Bus: storing {n} value(s) failed: {e}");
                }
            }
            (c, Err(e)) => {
                cursor = c;
                if health.connected() {
                    tracing::warn!("wM-Bus: {}: {e}", cfg.log.display());
                }
                health.set_connected(false);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("haus-wmbus-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    // rivet: verifies REQ-WMBUS-001
    #[test]
    fn the_log_is_followed_across_partial_lines_truncation_and_replacement() {
        let d = tmp("follow");
        let p = d.join("telegrams.log");
        std::fs::write(&p, "a\nb").unwrap();
        let mut c = Cursor::default();
        assert_eq!(read_new(&p, &mut c).unwrap(), vec!["a"]);
        // The rest of a line arrives later.
        std::fs::OpenOptions::new()
            .append(true)
            .open(&p)
            .unwrap()
            .write_all(b"c\nd\n")
            .unwrap();
        assert_eq!(read_new(&p, &mut c).unwrap(), vec!["bc", "d"]);
        assert!(read_new(&p, &mut c).unwrap().is_empty());
        // Truncated: read again from the start.
        std::fs::write(&p, "e\n").unwrap();
        assert_eq!(read_new(&p, &mut c).unwrap(), vec!["e"]);
        // Replaced by a new file (rotation: a new file renamed over it,
        // both exist at once, so the inode differs): from its start.
        let fresh = d.join("telegrams.log.new");
        std::fs::write(&fresh, "f\ng\n").unwrap();
        std::fs::rename(&fresh, &p).unwrap();
        assert_eq!(read_new(&p, &mut c).unwrap(), vec!["f", "g"]);
        let _ = std::fs::remove_dir_all(&d);
    }

    // rivet: verifies REQ-WMBUS-001
    #[test]
    fn both_settings_or_neither() {
        assert!(config(None, None).unwrap().is_none());
        assert!(config(Some("/x.log"), None)
            .unwrap_err()
            .starts_with("HAUS_WMBUS_LOG"));
        assert!(config(None, Some("/m.json"))
            .unwrap_err()
            .starts_with("HAUS_WMBUS_METERS"));
        let e = config(Some("/x.log"), Some("/nonexistent/m.json")).unwrap_err();
        assert!(
            e.starts_with("HAUS_WMBUS_METERS /nonexistent/m.json"),
            "{e}"
        );
    }
}
