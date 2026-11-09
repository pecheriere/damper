use super::*;

const GIB: u64 = 1 << 30;

fn machine() -> Machine {
    Machine {
        cores: 10,
        memory: 32 * GIB,
    }
}

#[test]
fn defaults_with_no_file() {
    let loaded = load(None, machine());
    assert_eq!(loaded.warnings, Vec::<String>::new());
    let e = loaded.effective;
    assert_eq!(e.cores, 10);
    assert_eq!(e.memory, 24 * GIB);
    assert_eq!(e.max_hold, Duration::from_secs(600));
    assert!(e.enabled);
    assert_eq!(e.priority, Priority::Utility);
    assert_eq!(e.extra_build_subcommands, Vec::<String>::new());
}

#[test]
fn empty_file_is_the_defaults() {
    assert_eq!(load(Some(""), machine()), load(None, machine()));
}

#[test]
fn default_memory_rounds_down() {
    let m = Machine {
        cores: 1,
        memory: 1001,
    };
    assert_eq!(load(None, m).effective.memory, 750);
}

#[test]
fn percent_and_absolute_values() {
    let m = machine();
    let e = load(Some("memory = \"24GiB\""), m).effective;
    assert_eq!(e.memory, 25_769_803_776);
    let e = load(Some("memory = \"50%\""), m).effective;
    assert_eq!(e.memory, 17_179_869_184);
    let e = load(Some("cores = \"50%\""), m).effective;
    assert_eq!(e.cores, 5);
    let e = load(Some("cores = 4"), m).effective;
    assert_eq!(e.cores, 4);
}

#[test]
fn small_core_percentage_keeps_one_core() {
    let m = Machine {
        cores: 4,
        memory: GIB,
    };
    assert_eq!(load(Some("cores = \"1%\""), m).effective.cores, 1);
}

#[test]
fn clamps_to_machine_with_warning() {
    let m = machine();
    let loaded = load(Some("memory = \"64GiB\""), m);
    assert_eq!(loaded.effective.memory, 32 * GIB);
    assert_eq!(loaded.warnings.len(), 1);
    let loaded = load(Some("cores = 20"), m);
    assert_eq!(loaded.effective.cores, 10);
    assert_eq!(loaded.warnings.len(), 1);
}

#[test]
fn rejects_zero_and_garbage_per_key() {
    let m = machine();
    let d = load(None, m).effective;
    let loaded = load(Some("cores = 0"), m);
    assert_eq!(loaded.effective, d);
    assert_eq!(loaded.warnings.len(), 1);
    assert!(loaded.warnings[0].contains("cores"));

    let loaded = load(Some("memory = \"lots\"\nmax_hold = \"5m\""), m);
    assert_eq!(loaded.effective.memory, d.memory);
    assert_eq!(loaded.effective.max_hold, Duration::from_secs(300));
    assert_eq!(loaded.warnings.len(), 1);
    assert!(loaded.warnings[0].contains("memory"));
}

#[test]
fn rejects_out_of_range_percentages_and_wrong_types() {
    let m = machine();
    let d = load(None, m).effective;
    for text in [
        "cores = \"0%\"",
        "cores = \"150%\"",
        "cores = -3",
        "cores = true",
        "memory = \"0\"",
        "memory = \"101%\"",
        "memory = 5",
        "max_hold = \"0s\"",
        "max_hold = 600",
        "enabled = \"yes\"",
        "priority = 1",
        "extra_build_subcommands = \"build\"",
        "extra_build_subcommands = [\"a b\"]",
        "extra_build_subcommands = [\"\"]",
    ] {
        let loaded = load(Some(text), m);
        assert_eq!(loaded.effective, d, "{text}");
        assert_eq!(loaded.warnings.len(), 1, "{text}");
    }
}

#[test]
fn invalid_toml_gives_defaults_and_one_warning() {
    let loaded = load(Some("cores = = ["), machine());
    assert_eq!(loaded.effective, load(None, machine()).effective);
    assert_eq!(loaded.warnings.len(), 1);
}

#[test]
fn unknown_key_warns() {
    let loaded = load(Some("colour = \"red\""), machine());
    assert_eq!(loaded.effective, load(None, machine()).effective);
    assert_eq!(loaded.warnings.len(), 1);
    assert!(loaded.warnings[0].contains("colour"));
}

#[test]
fn priority_values() {
    let m = machine();
    for (text, want) in [
        ("priority = \"utility\"", Priority::Utility),
        ("priority = \"nice\"", Priority::Nice),
        ("priority = \"none\"", Priority::None),
    ] {
        let loaded = load(Some(text), m);
        assert_eq!(loaded.effective.priority, want);
        assert_eq!(loaded.warnings, Vec::<String>::new());
    }
    let loaded = load(Some("priority = \"fast\""), m);
    assert_eq!(loaded.effective.priority, Priority::Utility);
    assert_eq!(loaded.warnings.len(), 1);
    assert!(loaded.warnings[0].contains("priority"));
}

#[test]
fn enabled_and_extra_subcommands() {
    let text = "enabled = false\nextra_build_subcommands = [\"zigbuild\", \"xwin\"]";
    let loaded = load(Some(text), machine());
    assert_eq!(loaded.warnings, Vec::<String>::new());
    assert!(!loaded.effective.enabled);
    assert_eq!(
        loaded.effective.extra_build_subcommands,
        ["zigbuild", "xwin"]
    );
}
