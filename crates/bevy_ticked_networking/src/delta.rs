//! Sending what changed.
//!
//! A full body every tick carried every component of every entity, most of them identical to
//! the tick before: two walking players cost 52 bytes a tick after the wire was made
//! entity-major, and most of those bytes said "still here, still the same". A delta carries,
//! per entity, only the components whose encoded bytes differ from a baseline the recipient
//! has acknowledged — plus the components removed and the entities despawned since it — and
//! the recipient rebuilds the full body from its copy of that baseline.
//!
//! # The baseline is what the recipient acked
//!
//! Every packet the server sends a client is a candidate baseline, kept in a ring per client;
//! a delta is built against the newest one the client has acknowledged (the `ack` on its input
//! packets), never against one it merely was sent. A lost delta costs nothing: the next delta
//! is against the same acked baseline, which the client still has. A client whose ack falls
//! off the ring, or that asks for one (`nack_full`), gets a full body; so does everybody every
//! `keyframe_every` packets, and a joiner on its first.
//!
//! # Classes and rates
//!
//! A [`ReplicationClass::Once`] type travels with an entity's first record and never again;
//! [`Always`](ReplicationClass::Always) travels on every delta. [`SendRates`] carries a type
//! only every nth delta: what it did not carry is what the client last had, which for a
//! slowly-changing type is the point and for anything else is a stale value — set it on what
//! can afford it.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::ops::Range;

use bevy::prelude::*;
use bevy_ticked::registry::{ReplicationClass, TickedComponentRegistry, TypeMask};

use crate::snapshot::{DeltaBody, EntityRecord, FullBody};

/// Where each component sits in a record's bytes, in wire order.
pub type Layout = Vec<(u16, Range<usize>)>;

/// A packet the server sent, kept to build deltas against once the client acknowledges it.
#[derive(Clone, Debug)]
pub struct Baseline {
    pub seq: u32,
    pub tick: u64,
    pub body: FullBody,
    /// One layout per record in `body.entities`.
    pub layouts: Vec<Layout>,
}

/// Per recipient, the last few packets sent, newest last.
#[derive(Resource, Default, Debug)]
pub struct Baselines(pub HashMap<u128, VecDeque<Baseline>>);

impl Baselines {
    pub fn get(&self, recipient: u128, seq: u32) -> Option<&Baseline> {
        self.0
            .get(&recipient)?
            .iter()
            .find(|baseline| baseline.seq == seq)
    }

    pub fn push(&mut self, recipient: u128, baseline: Baseline, keep: usize) {
        let ring = self.0.entry(recipient).or_default();
        ring.push_back(baseline);
        while ring.len() > keep.max(1) {
            ring.pop_front();
        }
    }

    pub fn forget(&mut self, recipient: u128) {
        self.0.remove(&recipient);
    }
}

/// How deltas are built. Runtime copy of the server plugin's fields.
#[derive(Resource, Clone, Copy, Debug)]
pub struct DeltaPolicy {
    /// Every this many packets to a client, a full body, delta or not.
    pub keyframe_every: u32,
    /// How many sent packets to keep per client as candidate baselines. An acknowledgement
    /// takes a round trip to come back, so the ring must hold that many packets or every
    /// packet is a keyframe: 32 covers half a second at 64 packets a second.
    pub max_unacked_baselines: usize,
    /// Whether to build deltas at all. Off is a full body every packet, as before.
    pub enabled: bool,
}

impl Default for DeltaPolicy {
    fn default() -> Self {
        Self {
            keyframe_every: 64,
            max_unacked_baselines: 32,
            enabled: true,
        }
    }
}

/// Per-type send rates for deltas: a type is carried only every nth delta. Opt-in.
#[derive(Resource, Default, Clone)]
pub struct SendRates {
    by_wire: BTreeMap<u16, u32>,
    pending: Vec<(fn(&TickedComponentRegistry) -> Option<u16>, u32)>,
}

impl SendRates {
    /// Carry `T` on every `every`th delta only.
    pub fn every<T: bevy_ticked::registry::TickedComponent>(mut self, every: u32) -> Self {
        fn index_of<T: bevy_ticked::registry::TickedComponent>(
            registry: &TickedComponentRegistry,
        ) -> Option<u16> {
            registry.wire_index_of::<T>()
        }
        self.pending.push((index_of::<T>, every.max(1)));
        self
    }

    /// Resolve type names to wire indices, once the registry can be read.
    pub fn resolve(&mut self, registry: &TickedComponentRegistry) {
        for (index_of, every) in self.pending.drain(..) {
            if let Some(index) = index_of(registry) {
                self.by_wire.insert(index, every);
            }
        }
    }

    fn carries(&self, wire_index: u16, seq: u32) -> bool {
        self.by_wire
            .get(&wire_index)
            .is_none_or(|every| seq.is_multiple_of(*every))
    }
}

/// Layouts for every record of `body`, from the registry.
pub fn layouts_of(registry: &TickedComponentRegistry, body: &FullBody) -> Vec<Layout> {
    body.entities
        .iter()
        .map(|record| {
            registry
                .split_record(&record.present, &record.bytes)
                .unwrap_or_default()
        })
        .collect()
}

/// What `current` says that `baseline` did not.
pub fn build_delta(
    registry: &TickedComponentRegistry,
    current: &FullBody,
    current_layouts: &[Layout],
    baseline: &Baseline,
    rates: &SendRates,
    seq: u32,
) -> DeltaBody {
    let base_by_id: HashMap<u64, (&EntityRecord, &Layout)> = baseline
        .body
        .entities
        .iter()
        .zip(&baseline.layouts)
        .map(|(record, layout)| (record.id, (record, layout)))
        .collect();

    let mut changed = Vec::new();
    let mut removed = Vec::new();
    for (record, layout) in current.entities.iter().zip(current_layouts) {
        let Some((base, base_layout)) = base_by_id.get(&record.id) else {
            // New to the recipient: everything it has.
            changed.push(record.clone());
            continue;
        };
        let base_parts: HashMap<u16, &[u8]> = base_layout
            .iter()
            .map(|(index, range)| (*index, &base.bytes[range.clone()]))
            .collect();
        let mut carried = EntityRecord::new(record.id);
        let mut any = false;
        for (index, range) in layout {
            let bytes = &record.bytes[range.clone()];
            let class = registry.class_of(*index).unwrap_or_default();
            let carry = match (base_parts.get(index), class) {
                (None, _) => true,
                (Some(_), ReplicationClass::Once) => false,
                (Some(_), ReplicationClass::Always) => rates.carries(*index, seq),
                (Some(before), ReplicationClass::Changed) => {
                    *before != bytes && rates.carries(*index, seq)
                }
            };
            if carry {
                carried.present.set(*index);
                carried.bytes.extend_from_slice(bytes);
                any = true;
            }
        }
        if any {
            changed.push(carried);
        }
        let mut gone = TypeMask::with_len(registry.wire_len());
        let mut any_gone = false;
        for index in base.present.iter() {
            if !record.present.contains(index) {
                gone.set(index);
                any_gone = true;
            }
        }
        if any_gone {
            removed.push((record.id, gone));
        }
    }
    let current_ids: std::collections::HashSet<u64> = current.ids().collect();
    let despawned: Vec<u64> = baseline
        .body
        .ids()
        .filter(|id| !current_ids.contains(id))
        .collect();

    let base_resources: HashMap<u16, &Vec<u8>> = baseline
        .body
        .resources
        .iter()
        .map(|(index, bytes)| (*index, bytes))
        .collect();
    let resources: Vec<(u16, Vec<u8>)> = current
        .resources
        .iter()
        .filter(|(index, bytes)| {
            base_resources
                .get(index)
                .is_none_or(|before| *before != bytes)
        })
        .cloned()
        .collect();

    DeltaBody {
        baseline_seq: baseline.seq,
        changed,
        removed,
        despawned,
        resources,
        inputs_ahead: current.inputs_ahead.clone(),
    }
}

/// The full body a delta describes, rebuilt from the baseline the recipient holds.
///
/// `None` if a record cannot be split (the registry disagrees with the sender's), which the
/// handshake makes impossible and this makes harmless.
pub fn apply_delta(
    registry: &TickedComponentRegistry,
    baseline: &FullBody,
    delta: &DeltaBody,
) -> Option<FullBody> {
    let despawned: std::collections::HashSet<u64> = delta.despawned.iter().copied().collect();
    let removed: HashMap<u64, &TypeMask> =
        delta.removed.iter().map(|(id, mask)| (*id, mask)).collect();
    let changed: HashMap<u64, &EntityRecord> = delta
        .changed
        .iter()
        .map(|record| (record.id, record))
        .collect();

    let mut out = FullBody {
        entities: Vec::with_capacity(baseline.entities.len() + delta.changed.len()),
        resources: Vec::new(),
        inputs_ahead: delta.inputs_ahead.clone(),
    };
    let wire_len = registry.wire_len();
    for base in &baseline.entities {
        if despawned.contains(&base.id) {
            continue;
        }
        let change = changed.get(&base.id);
        let gone = removed.get(&base.id);
        if change.is_none() && gone.is_none() {
            out.entities.push(base.clone());
            continue;
        }
        // Merge per component: the change wins, then the baseline, minus what was removed.
        let mut parts: BTreeMap<u16, Vec<u8>> = BTreeMap::new();
        for (index, range) in registry.split_record(&base.present, &base.bytes)? {
            if gone.is_some_and(|mask| mask.contains(index)) {
                continue;
            }
            parts.insert(index, base.bytes[range].to_vec());
        }
        if let Some(change) = change {
            for (index, range) in registry.split_record(&change.present, &change.bytes)? {
                parts.insert(index, change.bytes[range].to_vec());
            }
        }
        let mut record = EntityRecord::new(base.id);
        record.present = TypeMask::with_len(wire_len);
        for (index, bytes) in parts {
            record.present.set(index);
            record.bytes.extend_from_slice(&bytes);
        }
        out.entities.push(record);
    }
    // Entities the baseline did not have: the change is the whole record.
    let base_ids: std::collections::HashSet<u64> = baseline.ids().collect();
    for record in &delta.changed {
        if !base_ids.contains(&record.id) {
            out.put(record.clone());
        }
    }
    out.entities.sort_by_key(|record| record.id);

    let mut resources: BTreeMap<u16, Vec<u8>> = baseline.resources.iter().cloned().collect();
    for (index, bytes) in &delta.resources {
        resources.insert(*index, bytes.clone());
    }
    out.resources = resources.into_iter().collect();
    Some(out)
}

/// How a packet is compressed on the wire.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Compression {
    /// Never.
    None,
    /// LZ4, for packets over [`COMPRESS_ABOVE_BYTES`]. The default with the `lz4` feature.
    #[default]
    Lz4,
}

/// Packets at or below this many bytes go uncompressed: the header would cost more than the
/// saving, and a small packet is the common one.
pub const COMPRESS_ABOVE_BYTES: usize = 256;

const TAG_RAW: u8 = 0;
const TAG_LZ4: u8 = 1;

/// Wrap encoded packet bytes for the wire: a tag byte, then the bytes, compressed or not.
pub fn wrap(encoded: &[u8], compression: Compression) -> Vec<u8> {
    #[cfg(feature = "lz4")]
    if compression == Compression::Lz4 && encoded.len() > COMPRESS_ABOVE_BYTES {
        let compressed = lz4_flex::compress_prepend_size(encoded);
        if compressed.len() + 1 < encoded.len() + 1 {
            let mut out = Vec::with_capacity(compressed.len() + 1);
            out.push(TAG_LZ4);
            out.extend_from_slice(&compressed);
            return out;
        }
    }
    let _ = compression;
    let mut out = Vec::with_capacity(encoded.len() + 1);
    out.push(TAG_RAW);
    out.extend_from_slice(encoded);
    out
}

/// The encoded packet bytes inside a wire wrapper. `None` for a wrapper that is not one.
pub fn unwrap(wire: &[u8]) -> Option<std::borrow::Cow<'_, [u8]>> {
    match wire.split_first()? {
        (&TAG_RAW, rest) => Some(std::borrow::Cow::Borrowed(rest)),
        #[cfg(feature = "lz4")]
        (&TAG_LZ4, rest) => lz4_flex::decompress_size_prepended(rest)
            .ok()
            .map(std::borrow::Cow::Owned),
        _ => None,
    }
}
