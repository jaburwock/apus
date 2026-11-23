//! Approximate CIGAR insertion clusters for the pileup coverage bars.

use std::cmp::Reverse;
use std::collections::HashSet;
use std::ops::Range;

use crate::basepair::BasePair;

use super::read::ReadId;

const INSERTION_RADIUS: BasePair = BasePair(5);

pub(super) fn with_insertion_context(window: Range<BasePair>) -> Range<BasePair> {
    if window.is_empty() {
        return window;
    }
    (window.start - INSERTION_RADIUS)..(window.end + INSERTION_RADIUS)
}

pub(super) struct Insertion {
    pub anchor: BasePair,
    pub length: BasePair,
    pub read: (ReadId, BasePair),
}

struct Cluster {
    shortest: BasePair,
    longest: BasePair,
    observations: Vec<Insertion>,
}

impl Cluster {
    fn fits_length(&self, length: BasePair) -> bool {
        let shortest = self.shortest.min(length);
        let longest = self.longest.max(length);
        longest - shortest <= longest / 10
    }

    fn push(&mut self, observation: Insertion) {
        self.shortest = self.shortest.min(observation.length);
        self.longest = self.longest.max(observation.length);
        self.observations.push(observation);
    }

    fn median(&self) -> BasePair {
        let lower = self.observations[(self.observations.len() - 1) / 2].anchor;
        let upper = self.observations[self.observations.len() / 2].anchor;
        lower + (upper - lower) / 2
    }
}

/// Each cluster fits both tolerances end to end, so a chain of near neighbours cannot keep growing.
pub(super) fn cluster_support(mut observations: Vec<Insertion>, window: Range<BasePair>) -> Vec<(BasePair, u32)> {
    observations.sort_unstable_by_key(|ins| (ins.anchor, ins.length));
    let mut clusters: Vec<Cluster> = Vec::new();
    let mut first_active = 0;
    for ins in observations {
        while first_active < clusters.len()
            && ins.anchor - clusters[first_active].observations[0].anchor > INSERTION_RADIUS
        {
            first_active += 1;
        }
        if let Some(cluster) = clusters[first_active..].iter_mut().find(|c| c.fits_length(ins.length)) {
            cluster.push(ins);
        } else {
            clusters.push(Cluster {
                shortest: ins.length,
                longest: ins.length,
                observations: vec![ins],
            });
        }
    }

    let mut reads = HashSet::new();
    let mut points = Vec::new();
    for cluster in clusters {
        let anchor = cluster.median();
        if window.contains(&anchor) {
            reads.clear();
            reads.extend(cluster.observations.into_iter().map(|ins| ins.read));
            points.push((anchor, u32::try_from(reads.len()).unwrap_or(u32::MAX)));
        }
    }
    points.sort_unstable_by_key(|&(anchor, count)| (anchor, Reverse(count)));
    points.dedup_by_key(|(anchor, _)| *anchor);
    points
}
