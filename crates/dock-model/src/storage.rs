use std::{
    fs::{self, OpenOptions},
    io::Write,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

use crate::{
    model::{HandoffPacket, HandoffRecord, ReviewDecision},
    protocol::{DurablePaneQueue, DurableProgrammeGate},
    receipt::{RECEIPT_SCHEMA_VERSION, Receipt},
};
use serde::Serialize;

pub struct ProgrammeRecord {
    pub run_id: String,
    pub gate: Result<DurableProgrammeGate, String>,
}

/// One file found in the queue directory.
///
/// `identity` is the file's own name without its suffix, kept beside the parse result for the
/// same reason `ProgrammeRecord` keeps `run_id`: a record that failed to parse has no identity
/// except its filename, and quarantining it needs one.
pub struct PaneQueueRecord {
    pub identity: String,
    pub queue: Result<DurablePaneQueue, String>,
}

/// Handoffs that parsed, plus poison files that did not — skipped so one bad JSON cannot hide the rest.
pub struct HandoffInbox {
    pub records: Vec<HandoffRecord>,
    pub skipped: Vec<(String, String)>,
}

/// Receipts that parsed, plus the `(run_id, reason)` of poison files that did not. A bare tuple
/// return type trips `clippy::type_complexity`; naming it is the fix, not an exemption.
type ReceiptListing = Result<(Vec<Receipt>, Vec<(String, String)>), String>;

/// Where the queues live, and where a queue file that cannot be parsed goes instead.
const QUEUES: &str = "queues";
const QUEUE_QUARANTINE: &str = "queues-quarantine";

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
        Ok(self.list_handoff_inbox()?.records)
    }

    pub fn list_handoff_inbox(&self) -> Result<HandoffInbox, String> {
        let directory = self.root.join("handoffs");
        let entries = match fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(HandoffInbox {
                    records: Vec::new(),
                    skipped: Vec::new(),
                });
            }
            Err(error) => return Err(format!("could not read handoff inbox: {error}")),
        };
        let mut records = Vec::new();
        let mut skipped = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|error| format!("could not read handoff inbox: {error}"))?;
            let name = entry.file_name();
            let Some(run_id) = name.to_str().and_then(|n| n.strip_suffix(".json")) else {
                continue;
            };
            match self.load_handoff_record(run_id) {
                Ok(record) => records.push(record),
                Err(reason) => skipped.push((run_id.to_owned(), reason)),
            }
        }
        records.sort_by(|a, b| a.packet.run_id.cmp(&b.packet.run_id));
        skipped.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(HandoffInbox { records, skipped })
    }

    /// `.dock/local/receipts/<run_id>.json`, 0600, written once.
    ///
    /// Append-only in the sense that matters: `atomic_save` hard-links onto the destination and
    /// refuses an existing name, so a receipt cannot be rewritten after the fact by anything —
    /// including Dock.
    pub fn save_receipt(&self, receipt: &Receipt) -> Result<PathBuf, String> {
        if receipt.schema_version != RECEIPT_SCHEMA_VERSION {
            return Err("unsupported receipt schema version".into());
        }
        self.atomic_save("receipts", &receipt.run_id, receipt, CreateKind::Receipt)
    }

    pub fn load_receipt(&self, run_id: &str) -> Result<Receipt, String> {
        let receipt: Receipt = self.load("receipts", run_id)?;
        if receipt.run_id != run_id {
            return Err("stored receipt run_id does not match its requested filename".into());
        }
        Ok(receipt)
    }

    /// Every receipt that parsed, plus the ones that did not, so one corrupt file cannot hide
    /// the rest — the same contract `list_handoff_inbox` has, for the same reason.
    pub fn list_receipts(&self) -> ReceiptListing {
        let directory = self.root.join("receipts");
        let entries = match fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok((Vec::new(), Vec::new()));
            }
            Err(error) => return Err(format!("could not read receipts: {error}")),
        };
        let mut records = Vec::new();
        let mut skipped = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|error| format!("could not read receipts: {error}"))?;
            let name = entry.file_name();
            let Some(run_id) = name.to_str().and_then(|n| n.strip_suffix(".json")) else {
                continue;
            };
            match self.load_receipt(run_id) {
                Ok(receipt) => records.push(receipt),
                Err(reason) => skipped.push((run_id.to_owned(), reason)),
            }
        }
        records.sort_by(|a, b| a.run_id.cmp(&b.run_id));
        skipped.sort_by(|a, b| a.0.cmp(&b.0));
        Ok((records, skipped))
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

    /// Writes one pane's queue, replacing whatever was there.
    ///
    /// Deliberately not [`Self::atomic_save`], which links its temporary into place and therefore
    /// *refuses* to overwrite — exactly right for a handoff, which is a record of something that
    /// happened once, and exactly wrong for a queue, which is the current state of a thing and is
    /// rewritten on every add, remove and feed. The durability is the same: a temporary written
    /// and fsynced at `0600`, renamed over the destination, and the directory fsynced after, so a
    /// crash leaves either the old file or the new one and never half of either.
    pub fn save_pane_queue(&self, queue: &DurablePaneQueue) -> Result<PathBuf, String> {
        let filename = queue_filename(&queue.workspace_id, &queue.pane_id)?;
        let directory = self.root.join(QUEUES);
        fs::create_dir_all(&directory)
            .map_err(|error| format!("could not create queue storage: {error}"))?;
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))
            .map_err(|error| format!("could not secure queue storage: {error}"))?;
        let destination = directory.join(&filename);
        let temporary = directory.join(format!(
            ".{filename}.{}.tmp",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_err(|error| format!("system time is before Unix epoch: {error}"))?
                .as_nanos()
        ));
        let bytes = serde_json::to_vec_pretty(queue)
            .map_err(|error| format!("could not serialize queue: {error}"))?;
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)
            .map_err(|error| format!("could not create temporary queue file: {error}"))?;
        if let Err(error) = file.write_all(&bytes).and_then(|()| file.sync_all()) {
            let _ = fs::remove_file(&temporary);
            return Err(format!("could not persist queue: {error}"));
        }
        if let Err(error) = fs::rename(&temporary, &destination) {
            let _ = fs::remove_file(&temporary);
            return Err(format!("could not atomically save queue: {error}"));
        }
        sync_directory(&directory)?;
        Ok(destination)
    }

    /// Every queue file, each with its own parse result.
    ///
    /// A `Result` per record rather than one for the listing, exactly as
    /// [`Self::list_programme_gates`] does: one unreadable file must not cost the operator every
    /// other queue on the machine.
    pub fn list_pane_queues(&self) -> Result<Vec<PaneQueueRecord>, String> {
        let directory = self.root.join(QUEUES);
        let entries = match fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(format!("could not read queue storage: {error}")),
        };
        let mut queues = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|error| format!("could not read queue storage: {error}"))?;
            let Some(identity) = entry
                .file_name()
                .to_str()
                .and_then(|name| name.strip_suffix(".json"))
                .map(str::to_owned)
            else {
                continue;
            };
            let source = directory.join(format!("{identity}.json"));
            let queue = fs::read(&source)
                .map_err(|error| format!("could not read {}: {error}", source.display()))
                .and_then(|bytes| {
                    serde_json::from_slice(&bytes)
                        .map_err(|error| format!("could not parse {}: {error}", source.display()))
                })
                .and_then(|queue: DurablePaneQueue| {
                    // A file whose contents name a different pane than its own name does is not a
                    // queue this daemon can key anything by, whatever else is in it.
                    if queue_filename(&queue.workspace_id, &queue.pane_id)?
                        != format!("{identity}.json")
                    {
                        Err("stored queue does not match its filename".to_string())
                    } else if queue.schema_version != QUEUE_SCHEMA_VERSION {
                        Err(format!(
                            "stored queue is schema version {}; this daemon writes {QUEUE_SCHEMA_VERSION}",
                            queue.schema_version
                        ))
                    } else {
                        Ok(queue)
                    }
                });
            queues.push(PaneQueueRecord { identity, queue });
        }
        queues.sort_by(|a, b| a.identity.cmp(&b.identity));
        Ok(queues)
    }

    /// Moves a queue file that could not be parsed out of the way and leaves it there.
    ///
    /// Quarantine rather than deletion, and rather than refusing to start: the file is the only
    /// copy of work somebody queued, and a daemon that will not boot because one of them is
    /// unreadable is worse than one that boots without it and leaves the evidence on disk.
    pub fn quarantine_pane_queue(&self, identity: &str) -> Result<(), String> {
        if identity.is_empty() || identity.contains('/') || identity.contains('\\') {
            return Err("invalid queue quarantine identity".into());
        }
        let source_directory = self.root.join(QUEUES);
        let destination_directory = self.root.join(QUEUE_QUARANTINE);
        fs::create_dir_all(&destination_directory)
            .map_err(|error| format!("could not create queue quarantine: {error}"))?;
        fs::set_permissions(&destination_directory, fs::Permissions::from_mode(0o700))
            .map_err(|error| format!("could not secure queue quarantine: {error}"))?;
        let filename = format!("{identity}.json");
        let source = source_directory.join(&filename);
        let destination = destination_directory.join(filename);
        if destination.exists() {
            fs::remove_file(&source)
                .map_err(|error| format!("could not discard a duplicate invalid queue: {error}"))?;
        } else {
            fs::rename(&source, &destination)
                .map_err(|error| format!("could not quarantine an invalid queue: {error}"))?;
        }
        sync_directory(&source_directory)?;
        sync_directory(&destination_directory)
    }

    pub fn list_quarantined_pane_queue_ids(&self) -> Result<Vec<String>, String> {
        let directory = self.root.join(QUEUE_QUARANTINE);
        let entries = match fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(format!("could not read queue quarantine: {error}")),
        };
        let mut identities = Vec::new();
        for entry in entries {
            let entry =
                entry.map_err(|error| format!("could not read queue quarantine: {error}"))?;
            if let Some(identity) = entry
                .file_name()
                .to_str()
                .and_then(|name| name.strip_suffix(".json"))
            {
                identities.push(identity.to_owned());
            }
        }
        identities.sort();
        Ok(identities)
    }

    /// Forgets one pane's queue. Missing is success: a pane closed twice, or closed before it ever
    /// held an entry, is not a failure anybody can act on.
    pub fn remove_pane_queue(&self, workspace_id: &str, pane_id: &str) -> Result<(), String> {
        let directory = self.root.join(QUEUES);
        let destination = directory.join(queue_filename(workspace_id, pane_id)?);
        match fs::remove_file(&destination) {
            Ok(()) => sync_directory(&directory),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(format!("could not remove queue: {error}")),
        }
    }

    /// Records the daemon-wide kill switch.
    ///
    /// **Existence is the signal**, and the text inside is for whoever finds the file. Storing a
    /// parsed boolean would mean a truncated or half-written file has to be interpreted, and the
    /// only safe interpretation of "something is wrong with the pause flag" is *paused* — which is
    /// what a file that is merely present already says, with no parser to get it wrong.
    pub fn set_queue_pause(&self, paused: bool) -> Result<(), String> {
        let directory = self.root.join(QUEUES);
        fs::create_dir_all(&directory)
            .map_err(|error| format!("could not create queue storage: {error}"))?;
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))
            .map_err(|error| format!("could not secure queue storage: {error}"))?;
        let marker = directory.join("paused");
        if paused {
            let mut file = OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&marker)
                .map_err(|error| format!("could not persist the queue pause: {error}"))?;
            file.write_all(
                b"auto-feed is paused for the whole daemon; `dock queue resume` starts it again\n",
            )
            .and_then(|()| file.sync_all())
            .map_err(|error| format!("could not persist the queue pause: {error}"))?;
        } else if let Err(error) = fs::remove_file(&marker)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            return Err(format!("could not lift the queue pause: {error}"));
        }
        sync_directory(&directory)
    }

    /// Whether auto-feed was paused when the daemon last stopped.
    ///
    /// Persisted, unlike arming, and in the opposite direction: pausing before you walk away is a
    /// decision that must survive a restart, where arming is a decision that must not.
    pub fn queue_paused(&self) -> bool {
        self.root.join(QUEUES).join("paused").exists()
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
    Receipt,
}

impl CreateKind {
    const fn label(self) -> &'static str {
        match self {
            Self::Handoff => "handoff",
            Self::Decision => "decision",
            Self::ProgrammeGate => "programme gate",
            Self::Receipt => "receipt",
        }
    }
}

/// The schema this daemon writes, and the only one it will load.
pub const QUEUE_SCHEMA_VERSION: u16 = 1;

/// One queue's filename.
///
/// Both halves are validated to the same alphabet `packet_filename` uses, which is what makes the
/// `_` join safe to write and safe to check: the pair round-trips because the loaded file names
/// its own workspace and pane and the two are re-joined and compared, so an ambiguous split is
/// never attempted in the first place.
fn queue_filename(workspace_id: &str, pane_id: &str) -> Result<String, String> {
    for part in [workspace_id, pane_id] {
        if part.is_empty()
            || !part
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        {
            return Err(
                "queue identities must contain only letters, numbers, hyphens, or underscores"
                    .into(),
            );
        }
    }
    Ok(format!("{workspace_id}_{pane_id}.json"))
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
    use crate::model::{Check, HandoffEvidence};
    use crate::receipt::fixture as receipt_fixture;

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
    fn a_receipt_is_written_once_at_0600_and_read_back_whole() {
        let store = temporary_store("receipt-store");
        let receipt = receipt_fixture();
        let path = store.save_receipt(&receipt).expect("save receipt");
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(store.load_receipt(&receipt.run_id).unwrap(), receipt);
        // A second write of the same run is refused rather than silently overwriting evidence.
        assert!(store.save_receipt(&receipt).is_err());
    }

    #[test]
    fn listing_handoffs_skips_an_unreadable_file_rather_than_failing_the_inbox() {
        let store = temporary_store("handoff-skip");
        store
            .save_handoff_record(&HandoffRecord {
                packet: packet(),
                evidence: HandoffEvidence {
                    branch: "dock/fixture-handoff".into(),
                    base_sha: "aaa".into(),
                    head_sha: "bbb".into(),
                    status_entries: 1,
                    changed_files: 0,
                    untracked_files: 1,
                    insertions: 0,
                    deletions: 0,
                },
            })
            .expect("save");
        let directory = store.root.join("handoffs");
        fs::write(directory.join("dock_bad.json"), b"not json").expect("junk");
        let records = store
            .list_handoff_records()
            .expect("the inbox must still list");
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].packet.run_id, "dock_01J9");
        let inbox = store.list_handoff_inbox().expect("inbox");
        assert_eq!(inbox.skipped.len(), 1);
        assert_eq!(inbox.skipped[0].0, "dock_bad");
        let _ = fs::remove_dir_all(store.root);
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

    fn queue(pane_id: &str, entries: u64) -> DurablePaneQueue {
        DurablePaneQueue {
            schema_version: QUEUE_SCHEMA_VERSION,
            workspace_id: "dock".into(),
            pane_id: pane_id.into(),
            next_entry_id: entries + 1,
            entries: (1..=entries)
                .map(|entry_id| crate::protocol::DurableQueueEntry {
                    entry_id,
                    label: format!("card {entry_id}"),
                    prompt: "keep going".into(),
                })
                .collect(),
        }
    }

    /// The one place a queue must behave *unlike* a handoff. A handoff is a record of something
    /// that happened once and a second write of it is a bug; a queue is the current state of a
    /// thing and is rewritten on every add, remove and feed, so it renames over itself rather
    /// than linking into place.
    #[test]
    fn a_queue_is_rewritten_in_place_where_a_handoff_would_refuse() {
        let store = temporary_store("queue-rewrite");
        store
            .save_pane_queue(&queue("ledger", 2))
            .expect("first write");
        store
            .save_pane_queue(&queue("ledger", 1))
            .expect("second write");
        let records = store.list_pane_queues().expect("list queues");
        assert_eq!(
            records.len(),
            1,
            "a rewrite replaces rather than accumulates"
        );
        assert_eq!(
            records[0].queue.as_ref().expect("parses").entries.len(),
            1,
            "and the second write is what is there"
        );
        let _ = fs::remove_dir_all(&store.root);
    }

    /// A file whose contents name a different pane than its own name does is not a queue anything
    /// can be keyed by, whatever else is in it — so the ambiguity of splitting `workspace_pane` on
    /// an underscore is never attempted. The pair is re-joined and compared instead.
    #[test]
    fn a_queue_that_does_not_name_its_own_filename_is_refused() {
        let store = temporary_store("queue-mismatch");
        store.save_pane_queue(&queue("ledger", 1)).expect("write");
        let directory = store.root.join(QUEUES);
        let moved = serde_json::to_vec(&queue("elsewhere", 1)).unwrap();
        fs::write(directory.join("dock_ledger.json"), moved).expect("overwrite in place");
        let records = store.list_pane_queues().expect("list queues");
        assert!(
            records[0]
                .queue
                .as_ref()
                .expect_err("a mismatched queue must not load")
                .contains("filename")
        );
        store
            .quarantine_pane_queue(&records[0].identity)
            .expect("quarantine it");
        assert_eq!(
            store.list_quarantined_pane_queue_ids().unwrap(),
            vec!["dock_ledger".to_string()]
        );
        assert!(store.list_pane_queues().unwrap().is_empty());
        let _ = fs::remove_dir_all(&store.root);
    }

    /// The kill switch is a file that either exists or does not, so there is no content for a
    /// truncated write to make ambiguous. Lifting a pause that was never taken is not an error:
    /// `dock queue resume` on a running daemon is a reasonable thing for a person to type.
    #[test]
    fn the_pause_marker_is_its_own_existence_and_lifting_an_absent_one_is_not_an_error() {
        let store = temporary_store("queue-pause");
        assert!(!store.queue_paused());
        store.set_queue_pause(false).expect("lift an absent pause");
        assert!(!store.queue_paused());
        store.set_queue_pause(true).expect("pause");
        assert!(store.queue_paused());
        store.set_queue_pause(true).expect("pause again");
        assert!(store.queue_paused());
        store.set_queue_pause(false).expect("resume");
        assert!(!store.queue_paused());
        let _ = fs::remove_dir_all(&store.root);
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
