//! The record of one defect. A failure carries everything needed to act on it
//! — a minimal program, the pipeline that miscompiles it, and a command that
//! reproduces exactly this one failure — plus an `identity` chosen so that the
//! same defect found again from a different seed hashes to the same signature.
//!
//! The rendered issue body embeds the record itself, which makes an open issue
//! its own replay input: the nightly re-runs what it filed and closes what no
//! longer reproduces.

use serde::{Deserialize, Serialize};

const MARKER: &str = "fcc-fuzz-failure";

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct Failure {
    /// Nightly job that found it.
    pub job: String,
    /// One line naming the defect; becomes the issue title.
    pub summary: String,
    /// Exactly the bits that make two findings the same defect. Never a seed:
    /// seeds rotate nightly and would file a duplicate every night.
    pub identity: String,
    /// Command reproducing this one failure, not the run that found it.
    pub reproduce: String,
    /// What went wrong: expected versus actual, and where they first differ.
    pub details: String,
    /// The minimal artifact: a reduced program, or a crash input.
    pub artifact: String,
    /// Fence language for `artifact`.
    pub language: String,
}

impl Failure {
    /// Stable short hash of `identity`, used to match a finding against the
    /// issue already tracking it. FNV-1a rather than the standard hasher:
    /// this value is persisted in issue bodies and must not move when the
    /// toolchain does.
    pub fn signature(&self) -> String {
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        for byte in self.identity.as_bytes() {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x1000_0000_01b3);
        }
        format!("{hash:016x}")
    }

    pub fn title(&self) -> String {
        format!("{} [{}]", self.summary, self.signature())
    }

    pub fn body(&self) -> String {
        let Failure {
            job,
            reproduce,
            details,
            artifact,
            language,
            ..
        } = self;
        let record = serde_json::to_string(self).unwrap_or_default();
        // A fence needs its closer on its own line, and artifacts do not all
        // arrive newline-terminated.
        let artifact = artifact.strip_suffix('\n').unwrap_or(artifact);
        format!(
            "Found by the nightly `{job}` job.\n\
             \n\
             ## What happens\n\
             \n\
             {details}\n\
             \n\
             ## Reproduce\n\
             \n\
             ```sh\n{reproduce}\n```\n\
             \n\
             ## Case\n\
             \n\
             ```{language}\n{artifact}\n```\n\
             \n\
             ---\n\
             \n\
             This issue tracks one defect. Every night the case above is replayed; \
             the issue closes by itself once it stops reproducing, and no other \
             failure is filed against it.\n\
             \n\
             <!-- {MARKER}\n{record}\n-->\n"
        )
    }

    /// Recovers the record embedded in an issue body by [`Failure::body`].
    /// The record is one JSON line, so an artifact containing the comment
    /// terminator cannot truncate it.
    pub fn from_body(body: &str) -> Option<Self> {
        let mut lines = body.lines();
        lines.find(|line| line.trim() == format!("<!-- {MARKER}"))?;
        serde_json::from_str(lines.next()?).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn failure() -> Failure {
        Failure {
            job: "differential-fuzz".into(),
            summary: "sccp miscompiles a masked array store".into(),
            identity: "func.func(thread-state,sccp,erase-state)\nint main(void) { return 0; }\n"
                .into(),
            reproduce: "cargo xtask fcc-fuzz --seed 42 --iterations 1".into(),
            details: "line 1: expected \"7\", got \"9\"".into(),
            artifact: "int main(void) { return 0; }\n".into(),
            language: "c".into(),
        }
    }

    #[test]
    fn signature_tracks_identity_and_nothing_else() {
        let mut found_again = failure();
        found_again.summary = "same defect, different words".into();
        found_again.reproduce = "cargo xtask fcc-fuzz --seed 999 --iterations 1".into();
        assert_eq!(failure().signature(), found_again.signature());

        let mut other_defect = failure();
        other_defect.identity = "func.func(thread-state,dce,erase-state)\n".into();
        assert_ne!(failure().signature(), other_defect.signature());
    }

    #[test]
    fn issue_body_round_trips_the_record() {
        let body = failure().body();

        assert!(body.contains("int main(void) { return 0; }"));
        assert!(body.contains("cargo xtask fcc-fuzz --seed 42 --iterations 1"));
        assert_eq!(Failure::from_body(&body), Some(failure()));
    }
}
