//! The allocator defaults every binary linking this crate carries, whatever
//! its build path: jemalloc reads them from its `malloc_conf` symbol, which
//! this crate defines, so no build environment can lose them.
//!
//! jemalloc applies its option sources in order, each overriding the earlier
//! key by key: the `--with-malloc-conf` string compiled into the library, then
//! this symbol, then the name of the `/etc/_rjem_malloc.conf` symlink, then
//! `_RJEM_MALLOC_CONF`. An operator's environment therefore wins, and the
//! startup line reports what was resolved (ADR-0038).

/// The options: no huge-page refill of purged extents, freed pages returned
/// within 1 s, and four arenas whatever the host's CPU count.
pub const MALLOC_CONF: &str = "narenas:4,thp:never,dirty_decay_ms:1000,muzzy_decay_ms:1000";

const MALLOC_CONF_C: &[u8] = b"narenas:4,thp:never,dirty_decay_ms:1000,muzzy_decay_ms:1000\0";

/// jemalloc's `const char *malloc_conf`, under tikv-jemalloc-sys's `_rjem_`
/// prefix; a strong definition replaces the library's weak null one. An
/// `Option<&u8>` has the layout of a nullable C pointer.
///
/// SAFETY: jemalloc only reads the symbol, once, as a NUL-terminated string,
/// which `MALLOC_CONF_C` is; no other definition of the name exists.
#[cfg(not(target_env = "msvc"))]
#[used]
#[allow(unsafe_code)] // the one exported symbol jemalloc reads; see SAFETY above
#[unsafe(export_name = "_rjem_malloc_conf")]
static JE_MALLOC_CONF: Option<&u8> = Some(&MALLOC_CONF_C[0]);

#[cfg(test)]
mod defaults_tests {
    use super::*;

    #[test]
    fn the_c_string_is_the_options_nul_terminated() {
        assert_eq!(&MALLOC_CONF_C[..MALLOC_CONF_C.len() - 1], MALLOC_CONF.as_bytes());
        assert_eq!(MALLOC_CONF_C.last(), Some(&0));
    }
}
