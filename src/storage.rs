use std::{
    fs::{self, OpenOptions},
    io::Write,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

use crate::{
    model::{HandoffPacket, HandoffRecord, ReviewDecision},
    protocol::DurableProgrammeGate,
};
use serde::Serialize;

pub struct ProgrammeRecord {
    pub run_id: String,
    pub gate: Result<DurableProgrammeGate, String>,
}

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
        self.atomic_save("handoffs", &packet.run_id, packet, CreateKind::Handoff)
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

    pub fn save_handoff_record(&self, record: &HandoffRecord) -> Result<PathBuf, String> {
        record.packet.validate().map_err(str::to_owned)?;
        self.atomic_save(
            "handoffs",
            &record.packet.run_id,
            record,
            CreateKind::Handoff,
        )
    }

    pub fn load_handoff_record(&self, run_id: &str) -> Result<HandoffRecord, String> {
        let record: HandoffRecord = self.load("handoffs", run_id)?;
        record.packet.validate().map_err(str::to_owned)?;
        if record.packet.run_id != run_id {
            return Err("stored handoff run_id does not match its requested filename".into());
        }
        Ok(record)
    }

    pub fn list_handoff_records(&self) -> Result<Vec<HandoffRecord>, String> {
        let directory = self.root.join("handoffs");
        let entries = match fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(format!("could not read handoff inbox: {error}")),
        };
        let mut records = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|error| format!("could not read handoff inbox: {error}"))?;
            let name = entry.file_name();
            let Some(run_id) = name.to_str().and_then(|n| n.strip_suffix(".json")) else {
                continue;
            };
            records.push(self.load_handoff_record(run_id)?);
        }
        records.sort_by(|a, b| a.packet.run_id.cmp(&b.packet.run_id));
        Ok(records)
    }

    pub fn save_decision(&self, decision: &ReviewDecision) -> Result<PathBuf, String> {
        if decision.external_task_completed || decision.git_mutated {
            return Err("review decisions cannot complete tasks or mutate Git".into());
        }
        self.atomic_save(
            "decisions",
            &decision.run_id,
            decision,
            CreateKind::Decision,
        )
    }

    pub fn decision_exists(&self, run_id: &str) -> Result<bool, String> {
        Ok(self
            .root
            .join("decisions")
            .join(packet_filename(run_id)?)
            .exists())
    }

    pub fn load_decision(&self, run_id: &str) -> Result<ReviewDecision, String> {
        let decision: ReviewDecision = self.load("decisions", run_id)?;
        if decision.run_id != run_id || decision.external_task_completed || decision.git_mutated {
            return Err(
                "stored decision violates its exact run binding or authority boundary".into(),
            );
        }
        Ok(decision)
    }

    pub fn save_programme_gate(&self, gate: &DurableProgrammeGate) -> Result<PathBuf, String> {
        self.atomic_save(
            "programme-gates",
            &gate.dispatch.run_id,
            gate,
            CreateKind::ProgrammeGate,
        )
    }

    pub fn list_programme_gates(&self) -> Result<Vec<ProgrammeRecord>, String> {
        self.list_programme_records("programme-gates")
    }

    pub fn list_releasing_programme_gates(&self) -> Result<Vec<ProgrammeRecord>, String> {
        self.list_programme_records("programme-releases")
    }

    fn list_programme_records(&self, directory_name: &str) -> Result<Vec<ProgrammeRecord>, String> {
        let directory = self.root.join(directory_name);
        let entries = match fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(format!("could not read programme gates: {error}")),
        };
        let mut gates = Vec::new();
        for entry in entries {
            let entry =
                entry.map_err(|error| format!("could not read programme gates: {error}"))?;
            let Some(run_id) = entry
                .file_name()
                .to_str()
                .and_then(|name| name.strip_suffix(".json"))
                .map(str::to_owned)
            else {
                continue;
            };
            let gate = self
                .load(directory_name, &run_id)
                .and_then(|gate: DurableProgrammeGate| {
                    if gate.dispatch.run_id != run_id {
                        Err("stored programme gate run_id does not match its filename".into())
                    } else {
                        Ok(gate)
                    }
                });
            gates.push(ProgrammeRecord { run_id, gate });
        }
        gates.sort_by(|a, b| a.run_id.cmp(&b.run_id));
        Ok(gates)
    }

    pub fn quarantine_programme_gate(
        &self,
        source_directory: &str,
        run_id: &str,
    ) -> Result<(), String> {
        if run_id.is_empty() || run_id.contains('/') || run_id.contains('\\') {
            return Err("invalid programme gate quarantine identity".into());
        }
        let source_directory = self.root.join(source_directory);
        let destination_directory = self.root.join("programme-gate-quarantine");
        fs::create_dir_all(&destination_directory)
            .map_err(|error| format!("could not create programme gate quarantine: {error}"))?;
        fs::set_permissions(&destination_directory, fs::Permissions::from_mode(0o700))
            .map_err(|error| format!("could not secure programme gate quarantine: {error}"))?;
        let filename = format!("{run_id}.json");
        let source = source_directory.join(&filename);
        let destination = destination_directory.join(filename);
        if destination.exists() {
            fs::remove_file(&source).map_err(|error| {
                format!("could not terminalize duplicate invalid gate: {error}")
            })?;
        } else {
            fs::rename(&source, &destination)
                .map_err(|error| format!("could not quarantine invalid programme gate: {error}"))?;
        }
        sync_directory(&source_directory)?;
        sync_directory(&destination_directory)
    }

    pub fn list_quarantined_programme_gate_ids(&self) -> Result<Vec<String>, String> {
        let directory = self.root.join("programme-gate-quarantine");
        let entries = match fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(format!("could not read programme gate quarantine: {error}")),
        };
        let mut run_ids = Vec::new();
        for entry in entries {
            let entry = entry
                .map_err(|error| format!("could not read programme gate quarantine: {error}"))?;
            if let Some(run_id) = entry
                .file_name()
                .to_str()
                .and_then(|name| name.strip_suffix(".json"))
            {
                run_ids.push(run_id.to_owned());
            }
        }
        run_ids.sort();
        Ok(run_ids)
    }

    pub fn claim_programme_gate(&self, run_id: &str) -> Result<(), String> {
        self.move_programme_gate(run_id, "programme-gates", "programme-releases", false)
    }

    pub fn restore_programme_gate(&self, run_id: &str) -> Result<(), String> {
        self.move_programme_gate(run_id, "programme-releases", "programme-gates", true)
    }

    fn move_programme_gate(
        &self,
        run_id: &str,
        source_directory: &str,
        destination_directory: &str,
        destination_must_be_absent: bool,
    ) -> Result<(), String> {
        let filename = packet_filename(run_id)?;
        let source_directory = self.root.join(source_directory);
        let destination_directory = self.root.join(destination_directory);
        fs::create_dir_all(&destination_directory)
            .map_err(|error| format!("could not create programme release storage: {error}"))?;
        fs::set_permissions(&destination_directory, fs::Permissions::from_mode(0o700))
            .map_err(|error| format!("could not secure programme release storage: {error}"))?;
        let source = source_directory.join(&filename);
        let destination = destination_directory.join(filename);
        if destination_must_be_absent && destination.exists() {
            return Err(format!(
                "could not restore programme gate {run_id:?}: queued destination already exists"
            ));
        }
        fs::rename(&source, &destination)
            .map_err(|error| format!("could not persist programme gate release claim: {error}"))?;
        sync_directory(&source_directory)?;
        sync_directory(&destination_directory)
    }

    pub fn remove_programme_gate(&self, run_id: &str) -> Result<(), String> {
        self.remove_programme_record("programme-gates", run_id)
    }

    pub fn remove_releasing_programme_gate(&self, run_id: &str) -> Result<(), String> {
        self.remove_programme_record("programme-releases", run_id)
    }

    fn remove_programme_record(&self, directory: &str, run_id: &str) -> Result<(), String> {
        let destination = self.root.join(directory).join(packet_filename(run_id)?);
        fs::remove_file(&destination)
            .map_err(|error| format!("could not remove released programme gate: {error}"))?;
        let directory = destination
            .parent()
            .ok_or("programme gate has no parent directory")?;
        sync_directory(directory)
    }

    fn load<T: serde::de::DeserializeOwned>(
        &self,
        directory: &str,
        run_id: &str,
    ) -> Result<T, String> {
        let filename = packet_filename(run_id)?;
        let source = self.root.join(directory).join(filename);
        let bytes = fs::read(&source)
            .map_err(|error| format!("could not read {}: {error}", source.display()))?;
        serde_json::from_slice(&bytes)
            .map_err(|error| format!("could not parse {}: {error}", source.display()))
    }

    fn atomic_save<T: Serialize>(
        &self,
        directory: &str,
        run_id: &str,
        value: &T,
        kind: CreateKind,
    ) -> Result<PathBuf, String> {
        let filename = packet_filename(run_id)?;
        let directory = self.root.join(directory);
        fs::create_dir_all(&directory)
            .map_err(|error| format!("could not create local Dock storage: {error}"))?;
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))
            .map_err(|error| format!("could not secure local Dock storage: {error}"))?;
        let destination = directory.join(filename);
        let temporary = directory.join(format!(
            ".{run_id}.{}.tmp",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_err(|error| format!("system time is before Unix epoch: {error}"))?
                .as_nanos()
        ));
        let bytes = serde_json::to_vec_pretty(value)
            .map_err(|error| format!("could not serialize local record: {error}"))?;
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)
            .map_err(|error| format!("could not create temporary local record: {error}"))?;
        file.write_all(&bytes)
            .and_then(|_| file.sync_all())
            .map_err(|error| format!("could not persist local record: {error}"))?;
        if let Err(error) = fs::hard_link(&temporary, &destination) {
            let _ = fs::remove_file(&temporary);
            return if error.kind() == std::io::ErrorKind::AlreadyExists {
                Err(format!(
                    "a {} for run {run_id:?} already exists",
                    kind.label()
                ))
            } else {
                Err(format!("could not atomically save local record: {error}"))
            };
        }
        fs::remove_file(&temporary)
            .map_err(|error| format!("could not remove temporary local record: {error}"))?;
        fs::File::open(&directory)
            .and_then(|file| file.sync_all())
            .map_err(|error| format!("could not sync local record directory: {error}"))?;
        Ok(destination)
    }
}

fn sync_directory(directory: &std::path::Path) -> Result<(), String> {
    fs::File::open(directory)
        .and_then(|file| file.sync_all())
        .map_err(|error| format!("could not sync programme gate storage: {error}"))
}

#[derive(Clone, Copy)]
enum CreateKind {
    Handoff,
    Decision,
    ProgrammeGate,
}

impl CreateKind {
    const fn label(self) -> &'static str {
        match self {
            Self::Handoff => "handoff",
            Self::Decision => "decision",
            Self::ProgrammeGate => "programme gate",
        }
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

    #[test]
    fn a_second_handoff_cannot_replace_the_first_record() {
        let store = temporary_store("immutable-handoff");
        let first = packet();
        store.save_handoff(&first).expect("save first handoff");
        let mut replacement = first.clone();
        replacement.summary = "replacement evidence".into();

        let error = store
            .save_handoff(&replacement)
            .expect_err("duplicate must fail");
        assert!(error.contains("handoff") && error.contains("already exists"));
        assert_eq!(store.load_handoff(&first.run_id).unwrap(), first);
        let _ = fs::remove_dir_all(store.root);
    }
}
