use std::process::Command;

/// Compile-time build metadata, baked into the binary via `build.rs`.
#[derive(Debug, Clone)]
pub struct BuildProvenance {
    pub profile: String,
    pub opt_level: String,
    pub debug_assertions: bool,
    pub overflow_checks: bool,
    pub target_triple: String,
    pub rustc_version: String,
}

impl BuildProvenance {
    /// Construct from compile-time environment variables set by `build.rs`.
    pub fn capture() -> Self {
        Self {
            profile: env!("STACKS_BENCH_PROFILE").to_string(),
            opt_level: env!("STACKS_BENCH_OPT_LEVEL").to_string(),
            debug_assertions: env!("STACKS_BENCH_DEBUG_ASSERTIONS") == "true",
            overflow_checks: env!("STACKS_BENCH_OVERFLOW_CHECKS") == "true",
            target_triple: env!("STACKS_BENCH_TARGET").to_string(),
            rustc_version: env!("STACKS_BENCH_RUSTC_VERSION").to_string(),
        }
    }
}

/// Runtime git repository state. `None` fields indicate git was unavailable.
#[derive(Debug, Clone, Default)]
pub struct GitProvenance {
    pub branch: Option<String>,
    pub dirty: Option<bool>,
}

impl GitProvenance {
    /// Probe the current working directory for git state.
    pub fn capture() -> Self {
        Self {
            branch: Self::branch(),
            dirty: Self::dirty(),
        }
    }

    fn branch() -> Option<String> {
        Command::new("git")
            .args(["rev-parse", "--abbrev-ref", "HEAD"])
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .filter(|s| !s.is_empty())
    }

    fn dirty() -> Option<bool> {
        Command::new("git")
            .args(["status", "--porcelain"])
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| !o.stdout.is_empty())
    }
}

/// Combined build + repository provenance for a benchmark run.
#[derive(Debug, Clone)]
pub struct BenchmarkProvenance {
    pub build: BuildProvenance,
    pub git: GitProvenance,
}

impl BenchmarkProvenance {
    /// Capture all provenance in one call.
    pub fn capture() -> Self {
        Self {
            build: BuildProvenance::capture(),
            git: GitProvenance::capture(),
        }
    }
}
