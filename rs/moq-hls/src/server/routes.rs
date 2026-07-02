//! axum handlers for the HLS / LL-HLS endpoints.

use std::time::Duration;

use axum::Router;
use axum::extract::{Path, RawQuery, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use bytes::Bytes;

use super::Server;
use crate::export::Kind;
use crate::export::store::SegmentStore;

const M3U8: &str = "application/vnd.apple.mpegurl";
const MP4: &str = "video/mp4";

/// Playlists change every few hundred ms (LL-HLS), so a CDN or browser must
/// revalidate each time rather than serve a stale window.
const CACHE_PLAYLIST: &str = "no-cache";
/// Init segments and (partial) segments are immutable once produced and keyed by
/// a unique URL, so they can be cached indefinitely.
const CACHE_SEGMENT: &str = "public, max-age=31536000, immutable";

/// How far ahead of the last segment an LL-HLS blocking reload may ask before we
/// reject it (RFC 8216bis: last Media Sequence Number + 2).
const MAX_MSN_LEAD: u64 = 2;

/// How long a rendition lookup waits for the catalog to populate.
const READY_TIMEOUT: Duration = Duration::from_secs(5);
/// Upper bound on an LL-HLS blocking-reload / preload wait.
const BLOCK_TIMEOUT: Duration = Duration::from_secs(10);

pub fn router(server: Server) -> Router {
	Router::new()
		.route("/{broadcast}/master.m3u8", get(master))
		.route("/{broadcast}/{kind}/{rendition}/media.m3u8", get(media))
		.route("/{broadcast}/{kind}/{rendition}/init.mp4", get(init))
		.route("/{broadcast}/{kind}/{rendition}/seg/{file}", get(segment))
		.route("/{broadcast}/{kind}/{rendition}/part/{seq}/{file}", get(part))
		.with_state(server)
}

async fn master(State(server): State<Server>, Path(broadcast): Path<String>) -> Response {
	let Some(broadcaster) = server.broadcaster(&broadcast).await else {
		return not_found();
	};
	broadcaster.wait_ready(READY_TIMEOUT).await;
	m3u8(broadcaster.master_playlist())
}

async fn media(
	State(server): State<Server>,
	Path((broadcast, kind, rendition)): Path<(String, String, String)>,
	RawQuery(query): RawQuery,
) -> Response {
	let Some(kind) = Kind::from_path(&kind) else {
		return not_found();
	};
	let Some(store) = store(&server, &broadcast, kind, &rendition).await else {
		return not_found();
	};

	// LL-HLS blocking reload: wait until the requested (msn, part) lands.
	if let Some(msn) = query_param(query.as_deref(), "_HLS_msn").and_then(|v| v.parse::<u64>().ok()) {
		// A reload asking beyond the last segment + 2 can never be satisfied by the
		// live edge; the spec says reject it rather than block until timeout.
		if msn > store.version().last_sequence.saturating_add(MAX_MSN_LEAD) {
			return StatusCode::BAD_REQUEST.into_response();
		}
		let part = query_param(query.as_deref(), "_HLS_part")
			.and_then(|v| v.parse::<usize>().ok())
			.unwrap_or(0);
		block_until(&store, msn, part).await;
	}

	m3u8(crate::export::render_media(&store.snapshot()))
}

async fn init(
	State(server): State<Server>,
	Path((broadcast, kind, rendition)): Path<(String, String, String)>,
) -> Response {
	let Some(kind) = Kind::from_path(&kind) else {
		return not_found();
	};
	let Some(store) = store(&server, &broadcast, kind, &rendition).await else {
		return not_found();
	};
	match store.init() {
		Some(bytes) => media_bytes(bytes),
		None => not_found(),
	}
}

async fn segment(
	State(server): State<Server>,
	Path((broadcast, kind, rendition, file)): Path<(String, String, String, String)>,
) -> Response {
	let Some(kind) = Kind::from_path(&kind) else {
		return not_found();
	};
	let Some(sequence) = strip_m4s(&file).and_then(|s| s.parse::<u64>().ok()) else {
		return not_found();
	};
	let Some(store) = store(&server, &broadcast, kind, &rendition).await else {
		return not_found();
	};
	match store.segment(sequence) {
		Some(bytes) => media_bytes(bytes),
		None => not_found(),
	}
}

async fn part(
	State(server): State<Server>,
	Path((broadcast, kind, rendition, sequence, file)): Path<(String, String, String, u64, String)>,
) -> Response {
	let Some(kind) = Kind::from_path(&kind) else {
		return not_found();
	};
	let Some(index) = strip_m4s(&file).and_then(|s| s.parse::<usize>().ok()) else {
		return not_found();
	};
	let Some(store) = store(&server, &broadcast, kind, &rendition).await else {
		return not_found();
	};

	// The part may be a preload hint that hasn't been produced yet; block briefly.
	block_until(&store, sequence, index).await;

	match store.part(sequence, index) {
		Some(bytes) => media_bytes(bytes),
		None => not_found(),
	}
}

/// Resolve a rendition's store, waiting for the catalog to populate.
async fn store(server: &Server, broadcast: &str, kind: Kind, rendition: &str) -> Option<std::sync::Arc<SegmentStore>> {
	let broadcaster = server.broadcaster(broadcast).await?;
	broadcaster.wait_ready(READY_TIMEOUT).await;
	broadcaster.rendition(kind, rendition).map(|r| r.store.clone())
}

/// Block until the store holds `(msn, part)`, the window passed it, or the track
/// ended; bounded by [`BLOCK_TIMEOUT`].
async fn block_until(store: &SegmentStore, msn: u64, part: usize) {
	if store.satisfies(msn, part) {
		return;
	}
	let mut rx = store.subscribe();
	let _ = tokio::time::timeout(BLOCK_TIMEOUT, async {
		loop {
			if store.satisfies(msn, part) {
				break;
			}
			if rx.changed().await.is_err() {
				break;
			}
		}
	})
	.await;
}

/// Find a query parameter value in a raw `a=b&c=d` query string.
fn query_param<'a>(query: Option<&'a str>, key: &str) -> Option<&'a str> {
	query?.split('&').find_map(|pair| {
		let (k, v) = pair.split_once('=')?;
		(k == key).then_some(v)
	})
}

fn strip_m4s(file: &str) -> Option<&str> {
	file.strip_suffix(".m4s")
}

fn m3u8(body: String) -> Response {
	(
		[(header::CONTENT_TYPE, M3U8), (header::CACHE_CONTROL, CACHE_PLAYLIST)],
		body,
	)
		.into_response()
}

fn media_bytes(body: Bytes) -> Response {
	(
		[(header::CONTENT_TYPE, MP4), (header::CACHE_CONTROL, CACHE_SEGMENT)],
		body,
	)
		.into_response()
}

fn not_found() -> Response {
	StatusCode::NOT_FOUND.into_response()
}
