pub mod types;
pub use types::*;


// Design decisions
//
// Cancel/Modify of an unknown id:
//   Return Rejected { reason: UnknownOrder }. The order may have already filled
//   or been cancelled; the client must be told the cancel did nothing.
//   Never panic on bad input.
//
// Market order:
//   Match as much as possible against the opposite side, then cancel the
//   unfilled remainder (IOC behaviour). Market orders never rest in the book.
//   If nothing matches at all, return Rejected { reason: NoLiquidity }.