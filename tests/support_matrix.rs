//! Machine-check the support claims in `docs/SUPPORT.md` against the
//! controller profiles.
//!
//! The parser is deliberately simple: it finds the table row containing each
//! controller PID and checks the display name, PID and status word on that row.
//! It is meant to fail loudly when someone promotes (or demotes) a controller
//! in `src/profiles/` without updating the documentation, so the docs cannot
//! quietly overclaim again.

use switch2d::profiles::{all_profiles, profile_for, Support};
use switch2d::protocol::constants::{
    controller_name, JOYCON2_LEFT_PID, JOYCON2_RIGHT_PID, NSO_GAMECUBE_PID, PRO_CONTROLLER2_PID,
};

/// The four controllers the project claims to know about. A new profile must
/// be added here and to `docs/SUPPORT.md` in the same change.
const EXPECTED_PIDS: [u16; 4] = [
    JOYCON2_RIGHT_PID,
    JOYCON2_LEFT_PID,
    PRO_CONTROLLER2_PID,
    NSO_GAMECUBE_PID,
];

const SUPPORT_DOC: &str = include_str!("../docs/SUPPORT.md");

fn pid_token(pid: u16) -> String {
    // docs/SUPPORT.md writes the four ids as 0x2066 ..= 0x2073.
    format!("0x{pid:04X}")
}

/// Find the controller-table row that mentions `pid`.
fn support_row(pid: u16) -> &'static str {
    let token = pid_token(pid);
    SUPPORT_DOC
        .lines()
        .find(|line| line.starts_with('|') && line.contains(&token))
        .unwrap_or_else(|| {
            panic!(
                "docs/SUPPORT.md has no controller table row for {token} ({}); \
                 add the row or update the profile",
                controller_name(pid)
            )
        })
}

fn is_joycon(pid: u16) -> bool {
    pid == JOYCON2_RIGHT_PID || pid == JOYCON2_LEFT_PID
}

#[test]
fn profile_registry_matches_the_documented_four() {
    let profiles = all_profiles();
    assert_eq!(
        profiles.len(),
        EXPECTED_PIDS.len(),
        "docs/SUPPORT.md and the profile registry must cover the same models"
    );

    for pid in EXPECTED_PIDS {
        let profile = profile_for(pid)
            .unwrap_or_else(|| panic!("profile registry has no entry for {}", pid_token(pid)));
        assert_eq!(profile.pid, pid);
        assert_eq!(
            profile.name,
            controller_name(pid),
            "profile display name and protocol constants disagree for {}",
            pid_token(pid)
        );
    }
}

#[test]
fn support_rows_match_every_profile_pid_and_name() {
    for profile in all_profiles() {
        let token = pid_token(profile.pid);
        let name = profile.name;

        // The controller must be documented at all.
        assert!(
            SUPPORT_DOC.contains(&token),
            "docs/SUPPORT.md does not mention PID {token} ({name})"
        );
        assert!(
            SUPPORT_DOC.contains(name),
            "docs/SUPPORT.md does not mention display name {name:?} for {token}"
        );

        // The row must be self-consistent: PID and display name together.
        let row = support_row(profile.pid);
        assert!(
            row.contains(&token),
            "SUPPORT row for {name} does not contain {token}: {row}"
        );
        assert!(
            row.contains(name),
            "SUPPORT row for {token} does not contain display name {name:?}: {row}"
        );
    }
}

#[test]
fn only_joycon_rows_are_validated() {
    // exactly the two Joy-Con 2 rows may carry "validated"; Pro Controller 2
    // and NSO GameCube must stay "speculative" until real hardware evidence
    // exists. The profile's Support enum and the status word in the docs table
    // must agree, and the enum itself must follow the Joy-Con-only rule.
    let mut validated_rows = 0;

    for profile in all_profiles() {
        let row = support_row(profile.pid);
        let lower = row.to_ascii_lowercase();
        let token = pid_token(profile.pid);

        let documented = if lower.contains("validated") {
            "validated"
        } else if lower.contains("speculative") {
            "speculative"
        } else {
            panic!("SUPPORT row {token} has no validated/speculative status: {row}")
        };

        match (profile.support, is_joycon(profile.pid)) {
            (Support::Validated | Support::ValidatedWithKnownDefect(_), true) => {
                assert_eq!(
                    documented, "validated",
                    "Joy-Con row {token} must be documented as validated: {row}"
                );
                assert!(
                    !lower.contains("speculative"),
                    "Joy-Con row {token} must not be speculative: {row}"
                );
                validated_rows += 1;
            }
            (Support::Speculative, false) => {
                assert_eq!(
                    documented, "speculative",
                    "row {token} must be documented as speculative: {row}"
                );
                assert!(
                    !lower.contains("validated"),
                    "row {token} must not claim validated status: {row}"
                );
            }
            (support, joycon) => panic!(
                "{} ({token}) has support={support:?} but joycon={joycon}; \
                 only Joy-Con 2 rows may be validated",
                profile.name
            ),
        }
    }

    assert_eq!(
        validated_rows, 2,
        "exactly the two Joy-Con 2 rows may be validated; found {validated_rows}"
    );
}
