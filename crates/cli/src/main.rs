//! `yas`: the CLI ([`yas_cli`], which other programs carry too).

// glibc malloc retains freed memory in per-thread arenas (up to 8 per core);
// with one tokio worker per core this inflates RSS by hundreds of MB under
// streaming load. mimalloc returns memory far more aggressively.
#[cfg(feature = "mimalloc")]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn main() {
    yas_cli::main();
}
