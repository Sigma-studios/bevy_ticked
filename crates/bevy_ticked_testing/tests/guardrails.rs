//! Checking the checks: the source guard and the golden helper fail when they should.
//!
//! A guardrail nobody has seen fail is a guardrail nobody knows works. Every guard in this crate
//! is exercised here against synthetic sources in a scratch directory, both ways: it bites on the
//! reach, and it does not bite on a comment explaining the reach.

use bevy_ticked_testing::golden::{HashTrace, Trace, check_golden};
use bevy_ticked_testing::source_guard::{
    DEFAULT_NEEDLES, Exception, Needle, SourceGuard, code_only,
};
use std::fs;
use std::path::PathBuf;

/// A directory under the temp dir, removed on drop, including on a panic the test expected.
struct Scratch {
    dir: PathBuf,
}

impl Scratch {
    fn new(purpose: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "bevy_ticked_guardrails_{purpose}_{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("the temp dir is writable");
        Self { dir }
    }

    fn write(&self, relative: &str, contents: &str) {
        let path = self.dir.join(relative);
        fs::create_dir_all(path.parent().expect("a file has a parent"))
            .expect("the scratch dir is writable");
        fs::write(path, contents).expect("the scratch dir is writable");
    }

    fn guard(&self) -> SourceGuard {
        SourceGuard::new(&self.dir.to_string_lossy())
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

const SLEEP: Needle = Needle {
    needle: "sleep(",
    reason: "a tick does not wait",
};

#[test]
fn the_source_guard_bites() {
    SourceGuard::new(env!("CARGO_MANIFEST_DIR"))
        .ban(DEFAULT_NEEDLES)
        .assert_it_bites();
}

#[test]
fn the_default_needles_catch_every_known_reach() {
    // One reach per line, written the way it would be in a real system, so the needle spelling
    // is tested against the code it is meant to match and not against itself.
    let reaches: &[(&str, &str)] = &[
        ("Res<Time<Virtual>", "fn s(time: Res<Time<Virtual>>) {}"),
        ("Res<Time<Real>", "fn s(time: Res<Time<Real>>) {}"),
        ("Res<Time<Fixed>", "fn s(time: Res<Time<Fixed>>) {}"),
        ("ResMut<Time<", "fn s(mut time: ResMut<Time<Virtual>>) {}"),
        (
            "Time<Virtual>",
            "let delta = world.resource::<Time<Virtual>>().delta();",
        ),
        (
            "Time<Real>",
            "let delta = world.resource::<Time<Real>>().delta();",
        ),
        ("ButtonInput", "fn s(keys: Res<ButtonInput<KeyCode>>) {}"),
        ("rand::", "let x: u32 = rand::random();"),
        ("thread_rng", "let mut rng = thread_rng();"),
        ("rand::rng(", "let mut rng = rand::rng();"),
        ("SystemTime", "let now = SystemTime::now();"),
        ("Instant::now", "let now = Instant::now();"),
        (
            "Gizmos<",
            "fn s(mut gizmos: Gizmos<DefaultGizmoConfigGroup>) {}",
        ),
        (
            "Sprite",
            "commands.spawn(Sprite::from_color(Color::WHITE, Vec2::ONE));",
        ),
        ("AssetServer", "fn s(assets: Res<AssetServer>) {}"),
        (
            "SECONDS_PER_TICK",
            "velocity.0 += acceleration * SECONDS_PER_TICK;",
        ),
    ];
    for needle in DEFAULT_NEEDLES {
        assert!(
            reaches.iter().any(|(name, _)| name == &needle.needle),
            "DEFAULT_NEEDLES gained `{}`; add a reach for it to this test",
            needle.needle
        );
    }

    let scratch = Scratch::new("default_needles");
    let source = reaches
        .iter()
        .map(|(_, line)| *line)
        .collect::<Vec<_>>()
        .join("\n");
    scratch.write("src/sim.rs", &source);

    let violations = scratch
        .guard()
        .sources(&["src"])
        .ban(DEFAULT_NEEDLES)
        .violations();
    for (index, (needle, line)) in reaches.iter().enumerate() {
        let found = violations
            .iter()
            .find(|violation| violation.needle == *needle && violation.line == index + 1)
            .unwrap_or_else(|| panic!("`{needle}` did not catch `{line}`"));
        assert_eq!(found.path, "src/sim.rs");
        assert!(
            !found.reason.is_empty(),
            "`{needle}` has no reason; the violation would not say what to do instead"
        );
    }
}

#[test]
fn code_only_strips_comments_and_strings_but_keeps_lines() {
    let source = "let a = 1; // Instant::now() in a comment\n\
                  let b = \"Instant::now() in a string\";\n\
                  /* a block comment\n   spanning lines, with Instant::now()\n   and /* nested */ still a comment */ let c = 2;\n\
                  let quote = '\"'; let d = \"escaped \\\" quote, Instant::now()\";\n\
                  let url = \"http://not.a.comment\"; let now = Instant::now();\n\
                  let multi = \"a string\n   over two lines with Instant::now()\";\n";
    let code = code_only(source);

    assert_eq!(
        code.lines().count(),
        source.lines().count(),
        "code_only must keep the line count so violations carry the right line number"
    );
    let stripped: Vec<&str> = code.lines().collect();
    assert_eq!(stripped[0].trim(), "let a = 1;");
    assert_eq!(stripped[1].trim(), "let b = ;");
    assert_eq!(stripped[2].trim(), "");
    assert_eq!(stripped[3].trim(), "");
    assert_eq!(stripped[4].trim(), "let c = 2;");
    assert_eq!(stripped[5].trim(), "let quote = '\"'; let d = ;");
    assert_eq!(
        stripped[6].trim(),
        "let url = ; let now = Instant::now();",
        "a `//` inside a string is not a comment, and the code after the string is still code"
    );
    assert_eq!(stripped[7].trim(), "let multi =");
    assert_eq!(stripped[8].trim(), ";");

    assert_eq!(
        code.matches("Instant::now").count(),
        1,
        "only the real call survives"
    );
}

#[test]
#[should_panic(expected = "expired")]
fn an_expired_exception_fails() {
    let scratch = Scratch::new("expired");
    scratch.write("src/sim.rs", "std::thread::sleep(d);\n");
    let pending = Exception {
        path: "src/sim.rs",
        needle: "sleep(",
        reason: "until the tick stops waiting",
        expires: Some("2026-12-31"),
    };
    let guard = scratch
        .guard()
        .sources(&["src"])
        .ban(&[SLEEP])
        .allow(&[pending]);

    // Before the date it is a licence; the guard is clean and the exception is not expired.
    guard.assert_no_exception_has_expired_on("2026-09-11");
    guard.assert_clean();

    // The day after, it is a broken promise.
    guard.assert_no_exception_has_expired_on("2027-01-01");
}

#[test]
#[should_panic(expected = "delete the exception")]
fn a_stale_exception_fails() {
    let scratch = Scratch::new("stale");
    scratch.write("src/sim.rs", "std::thread::sleep(d);\n");
    scratch.write("src/fixed.rs", "// sleep( used to be here\n");
    let used = Exception {
        path: "src/sim.rs",
        needle: "sleep(",
        reason: "still there",
        expires: None,
    };
    let stale = Exception {
        path: "src/fixed.rs",
        needle: "sleep(",
        reason: "was fixed, exception forgotten",
        expires: None,
    };

    // A used exception excuses the reach and is still needed.
    let guard = scratch
        .guard()
        .sources(&["src"])
        .ban(&[SLEEP])
        .allow(&[used]);
    guard.assert_every_exception_is_still_needed();
    guard.assert_clean();

    // A stale one — the file only mentions the needle in a comment — must be deleted.
    scratch
        .guard()
        .sources(&["src"])
        .ban(&[SLEEP])
        .allow(&[used, stale])
        .assert_every_exception_is_still_needed();
}

#[test]
#[should_panic(expected = "no longer exists")]
fn a_stale_exclusion_fails() {
    let scratch = Scratch::new("exclusion");
    scratch.write("src/sim.rs", "fn tick() {}\n");
    scratch.write("src/visuals.rs", "std::thread::sleep(d);\n");

    let guard = scratch
        .guard()
        .sources(&["src"])
        .not_ticked(&["src/visuals.rs"])
        .ban(&[SLEEP]);
    guard.assert_excluded_paths_exist();
    guard.assert_clean();

    scratch
        .guard()
        .sources(&["src"])
        .not_ticked(&["src/renamed.rs"])
        .ban(&[SLEEP])
        .assert_excluded_paths_exist();
}

#[test]
#[should_panic(expected = "gone stale")]
fn a_stale_source_list_fails() {
    // A guard over a directory that no longer exists finds nothing, and nothing is clean.
    let scratch = Scratch::new("sources");
    scratch.write("src/sim.rs", "fn tick() {}\n");
    scratch
        .guard()
        .sources(&["src"])
        .ban(&[SLEEP])
        .min_files(1)
        .assert_clean();
    scratch
        .guard()
        .sources(&["simulation"])
        .ban(&[SLEEP])
        .assert_clean();
}

#[test]
fn a_golden_trace_round_trips() {
    let scratch = Scratch::new("golden");
    let trace = HashTrace {
        samples: vec![(1, 0x1234), (2, 0xdead_beef), (64, u64::MAX)],
    };

    // The text form is what a reviewer diffs, so it is pinned here as well as round-tripped.
    let text = trace.encode();
    assert_eq!(
        text,
        "1 0x0000000000001234\n2 0x00000000deadbeef\n64 0xffffffffffffffff\n"
    );
    assert_eq!(HashTrace::decode(&text).expect("decodes"), trace);

    // Written the way UPDATE_GOLDEN=1 would write it, then checked the way CI checks it.
    let path = scratch.dir.join("fixtures/trace.golden");
    fs::create_dir_all(path.parent().expect("has a parent")).expect("writable");
    fs::write(&path, text).expect("writable");
    check_golden(&path, &trace);

    // A trace that takes the default (postcard, hex) form round-trips too.
    #[derive(serde::Serialize, serde::Deserialize, PartialEq, Debug)]
    struct Positions(Vec<(u64, i32, i32)>);
    impl Trace for Positions {
        fn first_difference(&self, other: &Self) -> Option<String> {
            (self != other).then(|| "differs".to_string())
        }
    }
    let positions = Positions(vec![(1, -3, 4), (2, 0, 0)]);
    let text = positions.encode();
    assert!(
        text.trim().chars().all(|c| c.is_ascii_hexdigit()),
        "the default file form is hex: {text}"
    );
    assert_eq!(Positions::decode(&text).expect("decodes"), positions);
    let path = scratch.dir.join("fixtures/positions.golden");
    fs::write(&path, text).expect("writable");
    check_golden(&path, &positions);
}

#[test]
#[should_panic(expected = "tick 7")]
fn a_golden_difference_names_the_tick() {
    let recorded = HashTrace {
        samples: vec![(5, 1), (6, 2), (7, 3), (8, 4)],
    };
    let mut now = recorded.clone();
    now.samples[2].1 = 99;

    assert_eq!(recorded.first_difference(&recorded), None);
    let difference = now.first_difference(&recorded).expect("they differ");
    assert!(difference.starts_with("tick 7:"), "{difference}");

    // Length differences name the first tick one side does not have.
    let mut shorter = recorded.clone();
    shorter.samples.pop();
    let difference = shorter.first_difference(&recorded).expect("they differ");
    assert!(difference.starts_with("tick 8:"), "{difference}");
    let difference = recorded.first_difference(&shorter).expect("they differ");
    assert!(difference.starts_with("tick 8:"), "{difference}");

    // And check_golden says the same thing when it panics.
    let scratch = Scratch::new("golden_difference");
    let path = scratch.dir.join("trace.golden");
    fs::write(&path, recorded.encode()).expect("writable");
    check_golden(&path, &now);
}
