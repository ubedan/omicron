// align <alignment_function>
//
// where alignment_function takes one argument, an alignment period.
//
// The timestamps are set by:
//
// - end time of the query (now, if not specified, or last referenced time,
// e.g., `timestamp < @now() - 5m`).
// - Linearly spaced backwards from there by the alignment period.
//
// The alignment period in MQL can be set by a few things, such as an `every`
// operation; an alignment function which require the alignment period to be its
// window width; or graphics. The last doesn't matter for us, so use the other
// two.
//
// The function describes how to generate the values at each of those timestamps
// from the input. E.g., `interpolate()` does linear interpolation. Things like
// the delta() or rate() functions, do linear interpolation, then compute the
// difference between the end of each window and the beginning of it. Rate() is
// the same but divided by window width.
//
//
// TODO(ben):
//
// - Basic alignment, e.g. with interpolation or maybe the "every". Possibly
// rate. This requires keeping track of the query end time and a period (set by
// the period of the alignment function), and pushing that down to the split
// query.
// - group_by for distributions too, and keep integers separate if the reducer
// allows it (e.g., all but mean)
// - Maybe impl min / max / median reducers.
// - Push filters down into subqueries too, when splitting. e.g., split_query
// takes an optional filter, the output of the filter merging function.
