//! What may be used as a snapshot tag.
//!
//! A tag becomes a directory name on a node, so it has to be a plain name.
//! This lives in one place because the last time it did not, the result was
//! an advisory: `create_sandbox` joined the caller's tag into a path while
//! every sibling handler validated it, and the asymmetry was the bug. The
//! node checks again before it touches the filesystem -- that is the layer
//! that actually stops traversal -- but a bad tag should be refused at the
//! API with a 400 rather than accepted, written to the database, and failed
//! on a node with a message about a missing directory.

/// Whether `tag` may be used as a snapshot tag.
///
/// Deliberately a deny-list of path syntax rather than an allow-list of
/// characters: tags carry user-chosen names and restricting them to ASCII
/// would be a product decision, not a security one. What must not get
/// through is anything that changes which directory a tag names.
pub fn is_safe(tag: &str) -> bool {
    !tag.is_empty()
        && !tag.contains('/')
        && !tag.contains('\\')
        && !tag.contains("..")
        && !tag.starts_with('.')
        && !tag.contains('\0')
        && tag.len() <= 128
}

// ---- fetching a tag from the node that holds it ------------------------
//
// A fork can run on any node whose fingerprint matches the tag's, so a node
// that lacks the image fetches it from the node that baked it, over that
// node's agent port. The credential is not the pairing code: that is
// root-equivalent on the source node, and one node must not hold another's.
// It is a token for one tag, one fetching node and a deadline, signed with
// the source's pairing secret. The master can mint it because it holds that
// secret, and the source can check it without remembering anything.

/// Seconds a fetch token is good for at the least.
const FETCH_TTL: i64 = 600;
/// Expiries are rounded up to this, so the token -- and with it the Create
/// command that carries it -- stays byte-identical from one sync to the
/// next. A driver that saw a different command every sync would never
/// settle; it compares batches to decide whether anything changed.
const FETCH_BUCKET: i64 = 300;

/// When a fetch token minted at `now` should expire.
pub fn fetch_expiry(now: i64) -> i64 {
    (now + FETCH_TTL) / FETCH_BUCKET * FETCH_BUCKET + FETCH_BUCKET
}

fn fetch_mac(secret: &str, tag: &str, for_node: &str, expires: i64) -> hmac::Hmac<sha2::Sha256> {
    use hmac::Mac;
    let mut mac = <hmac::Hmac<sha2::Sha256>>::new_from_slice(secret.as_bytes())
        .expect("HMAC takes a key of any length");
    // Newline-separated: none of the fields can contain one -- a tag is a
    // plain name, a node id is the node's own, the expiry is digits -- so
    // no two different requests can sign the same bytes.
    mac.update(format!("tag-fetch\n{tag}\n{for_node}\n{expires}").as_bytes());
    mac
}

/// The token that lets `for_node` fetch `tag` from the node whose pairing
/// secret is `secret`, until `expires` (unix seconds).
pub fn fetch_token(secret: &str, tag: &str, for_node: &str, expires: i64) -> String {
    use hmac::Mac;
    hex::encode(
        fetch_mac(secret, tag, for_node, expires)
            .finalize()
            .into_bytes(),
    )
}

/// Whether `token` is one this node's `secret` signed for exactly this
/// tag, fetching node and expiry, and the expiry has not passed.
pub fn fetch_token_valid(
    secret: &str,
    tag: &str,
    for_node: &str,
    expires: i64,
    token: &str,
    now: i64,
) -> bool {
    use hmac::Mac;
    if expires < now {
        return false;
    }
    let Ok(given) = hex::decode(token) else {
        return false;
    };
    // verify_slice compares in constant time.
    fetch_mac(secret, tag, for_node, expires)
        .verify_slice(&given)
        .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fetch_token_is_good_for_its_tag_node_and_time_only() {
        let exp = 2_000_000_000;
        let t = fetch_token("secret", "py-base", "f5", exp);
        assert!(fetch_token_valid(
            "secret",
            "py-base",
            "f5",
            exp,
            &t,
            exp - 1
        ));
        assert!(fetch_token_valid("secret", "py-base", "f5", exp, &t, exp));

        assert!(
            !fetch_token_valid("secret", "py-base", "f5", exp, &t, exp + 1),
            "expired"
        );
        assert!(
            !fetch_token_valid("secret", "other", "f5", exp, &t, 0),
            "another tag"
        );
        assert!(
            !fetch_token_valid("secret", "py-base", "f7", exp, &t, 0),
            "another node"
        );
        assert!(
            !fetch_token_valid("secret", "py-base", "f5", exp + 300, &t, 0),
            "a later expiry"
        );
        assert!(
            !fetch_token_valid("other", "py-base", "f5", exp, &t, 0),
            "another source's secret"
        );
        assert!(
            !fetch_token_valid("secret", "py-base", "f5", exp, "zz", 0),
            "not hex"
        );
        assert!(
            !fetch_token_valid("secret", "py-base", "f5", exp, "", 0),
            "empty"
        );
    }

    #[test]
    fn the_expiry_holds_still_between_syncs_and_always_leaves_time() {
        // Every `now` inside one bucket mints the same expiry, so the token
        // and the command carrying it do not change between syncs.
        let base = 1_800_000_000 / FETCH_BUCKET * FETCH_BUCKET;
        let first = fetch_expiry(base);
        for dt in 0..FETCH_BUCKET {
            assert_eq!(fetch_expiry(base + dt), first);
        }
        assert_eq!(fetch_expiry(base + FETCH_BUCKET), first + FETCH_BUCKET);
        // And a token is never handed out with less than the TTL left.
        for now in base..base + 2 * FETCH_BUCKET {
            assert!(fetch_expiry(now) - now >= FETCH_TTL, "{now}");
        }
    }

    #[test]
    fn plain_names_pass() {
        for t in ["python-numpy", "v1.2.3", "feature_branch-2", "模板", "a"] {
            assert!(is_safe(t), "{t:?} should be allowed");
        }
    }

    #[test]
    fn anything_that_renames_the_directory_is_refused() {
        for t in [
            "",
            "..",
            "../etc",
            "a/../../etc/passwd",
            "sub/dir",
            "a\\b",
            ".hidden",
            "a\0b",
            // The 2026-05-17 advisory's shape: a tag that walks out of the
            // snapshot root and names a file the attacker wrote.
            "../../tmp/evil",
        ] {
            assert!(!is_safe(t), "{t:?} must be refused");
        }
    }

    #[test]
    fn a_tag_has_a_length_bound() {
        assert!(is_safe(&"a".repeat(128)));
        assert!(!is_safe(&"a".repeat(129)));
    }
}
