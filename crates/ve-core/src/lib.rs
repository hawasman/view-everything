//! The Index, Query, Category and size model shared by every part of view-everything.
//!
//! Vocabulary follows `CONTEXT.md`: an [`Index`] holds every [`Entry`](EntryId) of every
//! Indexed [`Volume`]; a [`Query`] selects which Entries count; a [`Filter`] holds the
//! Filtered Sizes that result.

mod bits;
pub mod category;
mod filter;
mod index;
pub mod query;

pub use bits::BitSet;
pub use category::{Category, CategoryMap};
pub use filter::{Filter, Totals};
pub use index::{EntryId, EntryMeta, Index, SyncState, Volume, VolumeId};
pub use query::{Candidate, Query, QueryContext, QueryError};
