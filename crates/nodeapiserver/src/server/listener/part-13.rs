
/// Parses the two watch-only `ListOptions` this listener can honor without
/// changing the cache protocol. `allowWatchBookmarks` controls delivery of
/// the cache driver's synthetic bookmark events; `timeoutSeconds` bounds the
/// complete stream, including a quiet watch, just as upstream's watch
/// handler does. Zero means no server-side timeout.
fn watch_options_query(query: &str) -> Result<WatchOptions, &'static str> {
    include!("body-18-1.rs");
    include!("body-18-2.rs");
}
