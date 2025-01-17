// Copyright 2017-2020 Parity Technologies (UK) Ltd.
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

//! Polkadot-specific GRANDPA integration utilities.

use std::sync::Arc;

use sp_runtime::traits::{Block as BlockT, NumberFor};
use sp_runtime::generic::BlockId;
use sp_runtime::traits::Header as _;

#[cfg(feature = "real-overseer")]
use {
	polkadot_primitives::v1::{Block as PolkadotBlock, Header as PolkadotHeader, BlockNumber},
	polkadot_subsystem::messages::ApprovalVotingMessage,
	prometheus_endpoint::{self, Registry},
	polkadot_overseer::OverseerHandler,
	futures::channel::oneshot,
};

/// A custom GRANDPA voting rule that acts as a diagnostic for the approval
/// voting subsystem's desired votes.
///
/// The practical effect of this voting rule is to implement a fixed delay of
/// blocks and to issue a prometheus metric on the lag behind the head that
/// approval checking would indicate.
#[cfg(feature = "real-overseer")]
#[derive(Clone)]
pub(crate) struct ApprovalCheckingDiagnostic {
	checking_lag: Option<prometheus_endpoint::Histogram>,
	overseer: OverseerHandler,
}

#[cfg(feature = "real-overseer")]
impl ApprovalCheckingDiagnostic {
	/// Create a new approval checking diagnostic voting rule.
	pub fn new(overseer: OverseerHandler, registry: Option<&Registry>)
		-> Result<Self, prometheus_endpoint::PrometheusError>
	{
		Ok(ApprovalCheckingDiagnostic {
			checking_lag: if let Some(registry) = registry {
				Some(prometheus_endpoint::register(
					prometheus_endpoint::Histogram::with_opts(
						prometheus_endpoint::HistogramOpts::new(
							"approval_checking_finality_lag",
							"How far behind the head of the chain the Approval Checking protocol wants to vote",
						).buckets(vec![1.0, 2.0, 3.0, 4.0, 5.0, 10.0, 20.0, 30.0, 40.0, 50.0])
					)?,
					registry,
				)?)
			} else {
				None
			},
			overseer,
		})
	}
}

#[cfg(feature = "real-overseer")]
impl<B> grandpa::VotingRule<PolkadotBlock, B> for ApprovalCheckingDiagnostic
	where B: sp_blockchain::HeaderBackend<PolkadotBlock>
{
	fn restrict_vote(
		&self,
		backend: Arc<B>,
		base: &PolkadotHeader,
		best_target: &PolkadotHeader,
		current_target: &PolkadotHeader,
	) -> grandpa::VotingRuleResult<PolkadotBlock> {
		// always wait 50 blocks behind the head to finalize.
		const DIAGNOSTIC_GRANDPA_DELAY: BlockNumber = 50;

		let aux = || {
			let find_target = |target_number: BlockNumber, current_header: &PolkadotHeader| {
				let mut target_hash = current_header.hash();
				let mut target_header = current_header.clone();

				loop {
					if *target_header.number() < target_number {
						unreachable!(
							"we are traversing backwards from a known block; \
							blocks are stored contiguously; \
							qed"
						);
					}

					if *target_header.number() == target_number {
						return Some((target_hash, target_number));
					}

					target_hash = *target_header.parent_hash();
					target_header = backend.header(BlockId::Hash(target_hash)).ok()?
						.expect("Header known to exist due to the existence of one of its descendents; qed");
				}
			};

			// delay blocks behind the head, but make sure we're not ahead of the current
			// target.
			let target_number = std::cmp::min(
				best_target.number().saturating_sub(DIAGNOSTIC_GRANDPA_DELAY),
				current_target.number().clone(),
			);

			// don't go below base
			let target_number = std::cmp::max(
				target_number,
				base.number().clone(),
			);

			find_target(target_number, current_target)
		};

		let actual_vote_target = aux();

		// Query approval checking and issue metrics.
		let mut overseer = self.overseer.clone();
		let checking_lag = self.checking_lag.clone();

		let best_hash = best_target.hash();
		let best_number = best_target.number.clone();

		let base_number = base.number;

		Box::pin(async move {
			let (tx, rx) = oneshot::channel();
			let approval_checking_subsystem_vote = {
				overseer.send_msg(ApprovalVotingMessage::ApprovedAncestor(
					best_hash,
					base_number,
					tx,
				)).await;

				rx.await.ok().and_then(|v| v)
			};

			let approval_checking_subsystem_lag = approval_checking_subsystem_vote.map_or(
				best_number - base_number,
				|(_h, n)| best_number - n,
			);

			if let Some(ref checking_lag) = checking_lag {
				checking_lag.observe(approval_checking_subsystem_lag as _);
			}

			tracing::debug!(
				target: "approval_voting",
				"GRANDPA: voting on {:?}. Approval-checking lag behind best is {}",
				actual_vote_target,
				approval_checking_subsystem_lag,
			);

			actual_vote_target
		})
	}
}

/// A custom GRANDPA voting rule that "pauses" voting (i.e. keeps voting for the
/// same last finalized block) after a given block at height `N` has been
/// finalized and for a delay of `M` blocks, i.e. until the best block reaches
/// `N` + `M`, the voter will keep voting for block `N`.
#[derive(Clone)]
pub(crate) struct PauseAfterBlockFor<N>(pub(crate) N, pub(crate) N);

impl<Block, B> grandpa::VotingRule<Block, B> for PauseAfterBlockFor<NumberFor<Block>>
where
	Block: BlockT,
	B: sp_blockchain::HeaderBackend<Block>,
{
	fn restrict_vote(
		&self,
		backend: Arc<B>,
		base: &Block::Header,
		best_target: &Block::Header,
		current_target: &Block::Header,
	) -> grandpa::VotingRuleResult<Block> {
		let aux = || {
			// walk backwards until we find the target block
			let find_target = |target_number: NumberFor<Block>, current_header: &Block::Header| {
				let mut target_hash = current_header.hash();
				let mut target_header = current_header.clone();

				loop {
					if *target_header.number() < target_number {
						unreachable!(
							"we are traversing backwards from a known block; \
							 blocks are stored contiguously; \
							 qed"
						);
					}

					if *target_header.number() == target_number {
						return Some((target_hash, target_number));
					}

					target_hash = *target_header.parent_hash();
					target_header = backend.header(BlockId::Hash(target_hash)).ok()?.expect(
						"Header known to exist due to the existence of one of its descendents; qed",
					);
				}
			};

			// only restrict votes targeting a block higher than the block
			// we've set for the pause
			if *current_target.number() > self.0 {
				// if we're past the pause period (i.e. `self.0 + self.1`)
				// then we no longer need to restrict any votes
				if *best_target.number() > self.0 + self.1 {
					return None;
				}

				// if we've finalized the pause block, just keep returning it
				// until best number increases enough to pass the condition above
				if *base.number() >= self.0 {
					return Some((base.hash(), *base.number()));
				}

				// otherwise find the target header at the pause block
				// to vote on
				return find_target(self.0, current_target);
			}

			None
		};

		let target = aux();

		Box::pin(async move { target })
	}
}

#[cfg(test)]
mod tests {
	use grandpa::VotingRule;
	use polkadot_test_client::{
		TestClientBuilder, TestClientBuilderExt, DefaultTestClientBuilderExt, InitPolkadotBlockBuilder,
		ClientBlockImportExt,
	};
	use sp_blockchain::HeaderBackend;
	use sp_runtime::{generic::BlockId, traits::Header};
	use consensus_common::BlockOrigin;
	use std::sync::Arc;

	#[test]
	fn grandpa_pause_voting_rule_works() {
		let _ = env_logger::try_init();

		let client = Arc::new(TestClientBuilder::new().build());

		let mut push_blocks = {
			let mut client = client.clone();

			move |n| {
				for _ in 0..n {
					let block = client.init_polkadot_block_builder().build().unwrap().block;
					client.import(BlockOrigin::Own, block).unwrap();
				}
			}
		};

		let get_header = {
			let client = client.clone();
			move |n| client.header(&BlockId::Number(n)).unwrap().unwrap()
		};

		// the rule should filter all votes after block #20
		// is finalized until block #50 is imported.
		let voting_rule = super::PauseAfterBlockFor(20, 30);

		// add 10 blocks
		push_blocks(10);
		assert_eq!(client.info().best_number, 10);

		// we have not reached the pause block
		// therefore nothing should be restricted
		assert_eq!(
			futures::executor::block_on(voting_rule.restrict_vote(
				client.clone(),
				&get_header(0),
				&get_header(10),
				&get_header(10)
			)),
			None,
		);

		// add 15 more blocks
		// best block: #25
		push_blocks(15);

		// we are targeting the pause block,
		// the vote should not be restricted
		assert_eq!(
			futures::executor::block_on(voting_rule.restrict_vote(
				client.clone(),
				&get_header(10),
				&get_header(20),
				&get_header(20)
			)),
			None,
		);

		// we are past the pause block, votes should
		// be limited to the pause block.
		let pause_block = get_header(20);
		assert_eq!(
			futures::executor::block_on(voting_rule.restrict_vote(
				client.clone(),
				&get_header(10),
				&get_header(21),
				&get_header(21)
			)),
			Some((pause_block.hash(), *pause_block.number())),
		);

		// we've finalized the pause block, so we'll keep
		// restricting our votes to it.
		assert_eq!(
			futures::executor::block_on(voting_rule.restrict_vote(
				client.clone(),
				&pause_block, // #20
				&get_header(21),
				&get_header(21),
			)),
			Some((pause_block.hash(), *pause_block.number())),
		);

		// add 30 more blocks
		// best block: #55
		push_blocks(30);

		// we're at the last block of the pause, this block
		// should still be considered in the pause period
		assert_eq!(
			futures::executor::block_on(voting_rule.restrict_vote(
				client.clone(),
				&pause_block, // #20
				&get_header(50),
				&get_header(50),
			)),
			Some((pause_block.hash(), *pause_block.number())),
		);

		// we're past the pause period, no votes should be filtered
		assert_eq!(
			futures::executor::block_on(voting_rule.restrict_vote(
				client.clone(),
				&pause_block, // #20
				&get_header(51),
				&get_header(51),
			)),
			None,
		);
	}
}
