// Copyright (C) Parity Technologies (UK) Ltd.
// This file is part of Polkadot.

// Polkadot is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Polkadot is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Polkadot.  If not, see <http://www.gnu.org/licenses/>.

//! Selection against stored roots and providers that can enact together.

use alloc::{
	collections::{BTreeMap, BTreeSet},
	vec::Vec,
};
use polkadot_primitives::{Hash, Id as ParaId, RequiresSet, StreamsRoot};

pub(super) type CandidateId = (ParaId, usize);

pub(super) struct Candidate {
	pub available: bool,
	pub provides: Option<StreamsRoot>,
	pub requires: Option<RequiresSet>,
}

/// Select before recording any new roots, so storage iteration order cannot change eligibility.
pub(super) fn select(
	pending: &BTreeMap<ParaId, Vec<Candidate>>,
	mut stored: impl FnMut(ParaId, &StreamsRoot) -> bool,
) -> BTreeSet<CandidateId> {
	let mut alive = BTreeSet::new();
	let mut providers = BTreeMap::new();
	for (&para, chain) in pending {
		for (index, candidate) in chain.iter().enumerate().take_while(|(_, c)| c.available) {
			alive.insert((para, index));
			if let Some(root) = candidate.provides {
				providers.insert((para, root.0), (para, index));
			}
		}
	}

	let mut dependents: BTreeMap<(ParaId, Hash), Vec<CandidateId>> = BTreeMap::new();
	let mut worklist = Vec::new();
	for &(para, index) in &alive {
		if let Some(requires) = &pending[&para][index].requires {
			for &(source, root) in requires.iter() {
				if stored(source, &root) {
					continue;
				}
				if providers.contains_key(&(source, root.0)) {
					dependents.entry((source, root.0)).or_default().push((para, index));
				} else {
					worklist.push((para, index));
					break;
				}
			}
		}
	}

	while let Some((para, index)) = worklist.pop() {
		if !alive.remove(&(para, index)) {
			continue;
		}
		if let Some(root) = pending[&para][index].provides {
			// Either duplicate withdrawing its root conservatively removes its dependents.
			providers.remove(&(para, root.0));
			worklist.extend(dependents.remove(&(para, root.0)).unwrap_or_default());
		}
		let successor = (para, index + 1);
		if alive.contains(&successor) {
			worklist.push(successor);
		}
	}
	alive
}

#[cfg(test)]
mod tests {
	use super::*;
	use alloc::vec;

	fn root(n: u8) -> StreamsRoot {
		StreamsRoot(Hash::repeat_byte(n))
	}

	fn candidate(provides: Option<u8>, requires: &[(u32, u8)]) -> Candidate {
		Candidate {
			available: true,
			provides: provides.map(root),
			requires: (!requires.is_empty()).then(|| {
				RequiresSet::try_from_iter(requires.iter().map(|&(p, r)| (p.into(), root(r))))
					.unwrap()
			}),
		}
	}

	#[test]
	fn dependencies_enact_together_or_are_removed_transitively() {
		let mut pending = BTreeMap::from([
			(1.into(), vec![candidate(Some(1), &[])]),
			(2.into(), vec![candidate(Some(2), &[(1, 1)]), candidate(None, &[])]),
			(3.into(), vec![candidate(None, &[(2, 2)])]),
		]);
		assert_eq!(select(&pending, |_, _| false).len(), 4);
		pending.get_mut(&1.into()).unwrap()[0].requires = candidate(None, &[(9, 9)]).requires;
		assert!(select(&pending, |_, _| false).is_empty());
		assert_eq!(select(&pending, |p, r| p == 9.into() && *r == root(9)).len(), 4);
	}

	#[test]
	fn cycles_survive_unless_an_outside_dependency_is_missing() {
		let mut pending = BTreeMap::from([
			(1.into(), vec![candidate(Some(1), &[(2, 2)])]),
			(2.into(), vec![candidate(Some(2), &[(1, 1)])]),
		]);
		assert_eq!(select(&pending, |_, _| false).len(), 2);
		pending.get_mut(&1.into()).unwrap()[0].requires =
			candidate(None, &[(2, 2), (9, 9)]).requires;
		assert!(select(&pending, |_, _| false).is_empty());
	}

	#[test]
	fn unavailable_predecessors_exclude_successor_providers() {
		let mut first = candidate(None, &[]);
		first.available = false;
		let pending = BTreeMap::from([
			(1.into(), vec![first, candidate(Some(1), &[])]),
			(2.into(), vec![candidate(None, &[(1, 1)])]),
		]);
		assert!(select(&pending, |_, _| false).is_empty());
		assert_eq!(select(&pending, |_, _| true), BTreeSet::from([(2.into(), 0)]));
	}

	#[test]
	fn removing_either_duplicate_withdraws_the_virtual_root() {
		for removed in 0..2 {
			let mut chain = vec![candidate(Some(1), &[]), candidate(Some(1), &[])];
			chain[removed].requires = candidate(None, &[(9, 9)]).requires;
			let pending =
				BTreeMap::from([(1.into(), chain), (2.into(), vec![candidate(None, &[(1, 1)])])]);
			let selected = select(&pending, |_, _| false);
			assert!(!selected.contains(&(2.into(), 0)));
			assert_eq!(selected.contains(&(1.into(), 0)), removed == 1);
			// A stored root does not depend on either virtual provider surviving.
			assert!(
				select(&pending, |p, r| p == 1.into() && *r == root(1)).contains(&(2.into(), 0))
			);
		}
	}

	/// Enumerate closed subsets independently of the removal worklist, with unique providers.
	#[test]
	fn selection_matches_exhaustive_closed_subsets() {
		for availability in 0..16 {
			for encoded in 0..6usize.pow(4) {
				let mut choices = encoded;
				let mut pending: BTreeMap<ParaId, Vec<Candidate>> = BTreeMap::new();
				let mut dependencies = [0; 4];
				for i in 0..4 {
					let dependency = choices % 6;
					choices /= 6;
					dependencies[i] = dependency;
					let requires = match dependency {
						0 => vec![],
						1 => vec![(9, 9)],
						d => vec![(((d - 2) / 2 + 1) as u32, (d - 1) as u8)],
					};
					let mut c = candidate(Some((i + 1) as u8), &requires);
					c.available = availability & (1 << i) != 0;
					pending.entry(((i / 2 + 1) as u32).into()).or_default().push(c);
				}
				let mut expected = BTreeSet::new();
				for subset in 0..16 {
					let valid = (0..4).filter(|i| subset & (1 << i) != 0).all(|i| {
						availability & (1 << i) != 0 &&
							(i % 2 == 0 || subset & (1 << (i - 1)) != 0) &&
							match dependencies[i] {
								0 => true,
								1 => false,
								d => subset & (1 << (d - 2)) != 0,
							}
					});
					if valid {
						for i in (0..4).filter(|i| subset & (1 << i) != 0) {
							expected.insert((((i / 2 + 1) as u32).into(), i % 2));
						}
					}
				}
				assert_eq!(select(&pending, |_, _| false), expected, "{availability}/{encoded}");
			}
		}
	}
}
