use std::{
    fs,
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

use crate::model::HandoffPacket;

#[derive(Debug, Clone)]
pub struct LocalStore {
    root: PathBuf,
}

impl LocalStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn save_handoff(&self, packet: &HandoffPacket) -> Result<PathBuf, String> {
        packet.validate().map_err(str::to_owned)?;
        let filename = packet_filename(&packet.run_id)?;
        let directory = self.root.join("handoffs");
        fs::create_dir_all(&directory)
            .map_err(|error| format!("could not create local Dock storage: {error}"))?;
        let destination = directory.join(filename);
        let temporary = directory.join(format!(
            ".{}.{}.tmp",
            packet.run_id,
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_err(|error| format!("system time is before Unix epoch: {error}"))?
                .as_nanos()
        ));
        let json = serde_json::to_vec_pretty(packet)
            .map_err(|error| format!("could not serialize handoff packet: {error}"))?;
        fs::write(&temporary, json)
            .map_err(|error| format!("could not write temporary handoff packet: {error}"))?;
        fs::rename(&temporary, &destination)
            .map_err(|error| format!("could not atomically save handoff packet: {error}"))?;
        Ok(destination)
    }

    pub fn load_handoff(&self, run_id: &str) -> Result<HandoffPacket, String> {
        let filename = packet_filename(run_id)?;
        let source = self.root.join("handoffs").join(filename);
        let json = fs::read(&source).map_err(|error| {
            format!(
                "could not read handoff packet {}: {error}",
                source.display()
            )
        })?;
        let packet: HandoffPacket = serde_json::from_slice(&json)
            .map_err(|error| format!("could not parse handoff packet: {error}"))?;
        packet.validate().map_err(str::to_owned)?;
        if packet.run_id != run_id {
            return Err("stored packet run_id does not match its requested filename".into());
        }
        Ok(packet)
    }
}

fn packet_filename(run_id: &str) -> Result<String, String> {
    if run_id.is_empty()
        || !run_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err("run_id must contain only letters, numbers, hyphens, or underscores".into());
    }
    Ok(format!("{run_id}.json"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Check;

    fn packet() -> HandoffPacket {
        HandoffPacket {
            schema_version: 1,
            run_id: "dock_01J9".into(),
            task_id: "DOCK-7".into(),
            workspace_id: "dock".into(),
            pane_id: "ledger-agent".into(),
            worktree: "/private/local-worktree".into(),
            branch: "dock/fixture-handoff".into(),
            base_sha: "3fa91c2".into(),
            summary: "Bounded explicit handoff.".into(),
            question: None,
            checks: vec![Check {
                name: "cargo test".into(),
                passed: true,
            }],
        }
    }

    fn temporary_store(test_name: &str) -> LocalStore {
        let path =
            std::env::temp_dir().join(format!("dock-storage-{test_name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        LocalStore::new(path)
    }

    #[test]
    fn packet_round_trip_uses_a_local_run_id_filename() {
        let store = temporary_store("round-trip");
        let expected = packet();
        let location = store.save_handoff(&expected).expect("save packet");
        assert!(location.ends_with("handoffs/dock_01J9.json"));
        assert_eq!(
            store.load_handoff("dock_01J9").expect("load packet"),
            expected
        );
        let _ = fs::remove_dir_all(store.root);
    }

    #[test]
    fn traversal_like_run_ids_are_rejected_before_io() {
        let store = temporary_store("traversal");
        assert!(store.load_handoff("../outside").is_err());
        assert!(packet_filename("has space").is_err());
    }

    #[test]
    fn corrupt_or_mismatched_packets_are_rejected() {
        let store = temporary_store("corrupt");
        let directory = store.root.join("handoffs");
        fs::create_dir_all(&directory).expect("create store");
        fs::write(directory.join("dock_01J9.json"), b"not json").expect("write corrupt packet");
        assert!(store.load_handoff("dock_01J9").is_err());
    }
}
