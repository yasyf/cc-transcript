use std::sync::Arc;

use cc_transcript_core::toolcall::ToolRegistrySnapshot;

use cc_transcript_core::types::{
    AssistantEntry, AttachmentEntry, ContentBlock, Entry, EntryMeta, ModeEntry, OtherEntry,
    PrintBody, PrintResult, SystemEntry, Usage, UserContent, UserEntry,
};

/// One event's slot in the shared parse output: every event-scoped view is an
/// `Arc<Vec<Entry>>` plus an index, resolved fresh on each getter access.
#[derive(Clone)]
pub(crate) struct EventRef {
    pub entries: Arc<Vec<Entry>>,
    pub idx: usize,
    pub registry: Option<Arc<ToolRegistrySnapshot>>,
}

impl EventRef {
    pub fn new(entries: Arc<Vec<Entry>>, idx: usize) -> Self {
        Self {
            entries,
            idx,
            registry: ToolRegistrySnapshot::capture_scoped(),
        }
    }

    pub fn entry(&self) -> &Entry {
        &self.entries[self.idx]
    }

    pub fn meta(&self) -> &EntryMeta {
        self.entry()
            .meta()
            .expect("meta view over an enveloped entry")
    }

    pub fn user(&self) -> &UserEntry {
        match self.entry() {
            Entry::User(user) => user,
            _ => unreachable!("user view over a non-user entry"),
        }
    }

    pub fn assistant(&self) -> &AssistantEntry {
        match self.entry() {
            Entry::Assistant(assistant) => assistant,
            _ => unreachable!("assistant view over a non-assistant entry"),
        }
    }

    pub fn system(&self) -> &SystemEntry {
        match self.entry() {
            Entry::System(system) => system,
            _ => unreachable!("system view over a non-system entry"),
        }
    }

    pub fn mode(&self) -> &ModeEntry {
        match self.entry() {
            Entry::Mode(mode) => mode,
            _ => unreachable!("mode view over a non-mode entry"),
        }
    }

    pub fn other(&self) -> &OtherEntry {
        match self.entry() {
            Entry::Other(other) => other,
            _ => unreachable!("other view over a non-other entry"),
        }
    }

    pub fn attachment(&self) -> &AttachmentEntry {
        match self.entry() {
            Entry::Attachment(attachment) => attachment,
            _ => unreachable!("attachment view over a non-attachment entry"),
        }
    }
}

/// Where a content block lives: an entry's block list, or one message inside a
/// parsed ``--print`` envelope.
#[derive(Clone)]
pub(crate) enum BlockHost {
    Owned(Arc<Vec<ContentBlock>>),
    Entry(EventRef),
    Print(Arc<PrintResult>, usize),
}

impl BlockHost {
    pub fn registry(&self) -> Option<Arc<ToolRegistrySnapshot>> {
        match self {
            Self::Entry(event) if event.registry.is_some() => event.registry.clone(),
            _ => ToolRegistrySnapshot::capture_scoped(),
        }
    }

    pub fn blocks(&self) -> &[ContentBlock] {
        match self {
            BlockHost::Owned(blocks) => blocks,
            BlockHost::Entry(r) => r.entry().blocks(),
            BlockHost::Print(pr, msg) => match &pr.messages[*msg].body {
                PrintBody::Assistant { blocks, .. } => blocks,
                PrintBody::User(UserContent::Blocks(blocks)) => blocks,
                PrintBody::User(UserContent::Plain(_)) => &[],
            },
        }
    }
}

#[derive(Clone)]
pub(crate) struct BlockRef {
    pub host: BlockHost,
    pub block: usize,
    pub registry: Option<Arc<ToolRegistrySnapshot>>,
}

pub(crate) fn with_view_registry<R>(
    registry: &Option<Arc<ToolRegistrySnapshot>>,
    operation: impl FnOnce() -> R,
) -> R {
    match registry {
        Some(registry) => {
            cc_transcript_core::toolcall::with_registry(Arc::clone(registry), operation)
        }
        None => operation(),
    }
}

impl BlockRef {
    pub fn with_registry<R>(&self, operation: impl FnOnce() -> R) -> R {
        with_view_registry(&self.registry, operation)
    }

    pub fn block(&self) -> &ContentBlock {
        &self.host.blocks()[self.block]
    }
}

/// Where a Usage lives: an assistant entry's optional usage, the aggregate
/// usage of a ``--print`` result, or one ``--print`` message's usage.
#[derive(Clone)]
pub(crate) enum UsageHost {
    Entry(EventRef),
    Print(Arc<PrintResult>),
    PrintMessage(Arc<PrintResult>, usize),
}

impl UsageHost {
    pub fn usage(&self) -> &Usage {
        match self {
            UsageHost::Entry(r) => r
                .assistant()
                .usage
                .as_ref()
                .expect("usage view over an assistant entry with usage"),
            UsageHost::Print(pr) => &pr.usage,
            UsageHost::PrintMessage(pr, msg) => pr.messages[*msg]
                .usage
                .as_ref()
                .expect("usage view over a print message with usage"),
        }
    }
}

#[derive(Clone)]
pub(crate) struct PrintRef(pub Arc<PrintResult>);
