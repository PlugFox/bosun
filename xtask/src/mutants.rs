//! The mutant job of #12, with the runner rules of #15.
//!
//! A mutant is a check that must go red when a mechanism is disabled. Each one is a cfg
//! (`--cfg mutant_<name>`, never a cargo feature), the package whose code it changes, and the
//! check that must go red. For each mutant the runner, both without and with the cfg:
//!
//! 1. builds the package's tests (`cargo test -p <package> --no-run`), which must succeed: a
//!    mutant that merely breaks the build is not red, it is broken;
//! 2. runs the check, which must pass without the cfg and fail with it.
//!
//! Every name registered here is also declared in the workspace's `unexpected_cfgs` lint and
//! used in the source; a test asserts the three name the same set.

use std::path::{Path, PathBuf};
use std::process::Command;

/// The check that must go red under a mutant.
#[derive(Debug, Clone, Copy)]
enum Check {
    /// `cargo test -p <package> -- <test> --exact`: one named test.
    Test(&'static str),
    /// Arguments to `cargo`, for checks that are commands rather than tests.
    Cargo(&'static [&'static str]),
}

/// A host platform, for mutants whose check can only fail on one of them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Platform {
    Windows,
    Macos,
    Linux,
}

impl Platform {
    fn host() -> Option<Self> {
        if cfg!(windows) {
            Some(Self::Windows)
        } else if cfg!(target_os = "macos") {
            Some(Self::Macos)
        } else if cfg!(target_os = "linux") {
            Some(Self::Linux)
        } else {
            None
        }
    }
}

#[derive(Debug)]
struct Mutant {
    /// The cfg name, `mutant_<what is broken>`.
    name: &'static str,
    /// The issue whose acceptance list names this mutant.
    issue: u32,
    /// What the cfg breaks.
    breaks: &'static str,
    /// The package whose code the cfg changes; its tests are built first.
    package: &'static str,
    /// The check that must go red.
    check: Check,
    /// The only platform where the check can go red; `None` for any.
    platform: Option<Platform>,
}

const MUTANTS: &[Mutant] = &[
    Mutant {
        name: "mutant_core_async_runtime",
        issue: 12,
        breaks: "bosun-core depends on the async runtime",
        package: "bosun-core",
        check: Check::Cargo(&["xtask", "check-deps"]),
        platform: None,
    },
    Mutant {
        name: "mutant_xtask_no_build_step",
        issue: 15,
        breaks: "the mutant runner accepts a mutant that does not build",
        package: "xtask",
        check: Check::Test("mutants::tests::a_build_failure_is_not_red"),
        platform: None,
    },
    Mutant {
        name: "mutant_core_1h_as_5m",
        issue: 17,
        breaks: "1 h cache writes are priced as 5 min",
        package: "bosun-core",
        check: Check::Test("price::tests::cost_prices_each_category"),
        platform: None,
    },
    Mutant {
        name: "mutant_core_final_blocks_only",
        issue: 17,
        breaks: "a merged message keeps the blocks of its final record only",
        package: "bosun-core",
        check: Check::Test("merge::tests::split_message_keeps_final_usage_and_every_tool_call"),
        platform: None,
    },
    Mutant {
        name: "mutant_core_first_record_wins",
        issue: 17,
        breaks: "a merged message keeps the usage of the first record seen",
        package: "bosun-core",
        check: Check::Test("merge::tests::merge_is_order_independent"),
        platform: None,
    },
    Mutant {
        name: "mutant_core_tie_by_file",
        issue: 6,
        breaks: "a timestamp tie goes to the smaller file key, not the smaller session id",
        package: "bosun-core",
        check: Check::Test(
            "merge::tests::a_timestamp_tie_goes_to_the_smaller_session_not_the_smaller_file",
        ),
        platform: None,
    },
    Mutant {
        name: "mutant_core_no_threshold",
        issue: 17,
        breaks: "the prompt-size price row is ignored",
        package: "bosun-core",
        check: Check::Test("price::tests::prompts_over_the_threshold_use_the_threshold_row"),
        platform: None,
    },
    Mutant {
        name: "mutant_core_case_sensitive",
        issue: 17,
        breaks: "Windows path canonicalisation keeps case",
        package: "bosun-core",
        check: Check::Test("path::tests::windows_paths_differing_by_case_or_separator_share_a_key"),
        platform: None,
    },
    Mutant {
        name: "mutant_recount_last_block",
        issue: 16,
        breaks: "recount takes the last usage block per session instead of the sum",
        package: "recount",
        check: Check::Test("tests::every_fixture_equals_its_expected"),
        platform: None,
    },
    Mutant {
        name: "mutant_recount_no_dedup",
        issue: 16,
        breaks: "recount sums every record instead of every message",
        package: "recount",
        check: Check::Test("tests::a_split_message_counts_once"),
        platform: None,
    },
    Mutant {
        name: "mutant_recount_tie_by_path",
        issue: 6,
        breaks: "recount breaks a timestamp tie by path instead of by session id",
        package: "recount",
        check: Check::Test("tests::a_timestamp_tie_goes_to_the_smaller_session"),
        platform: None,
    },
    Mutant {
        name: "mutant_fixture_lint_off",
        issue: 16,
        breaks: "lint-fixtures accepts everything",
        package: "xtask",
        check: Check::Test("fixtures::tests::a_bad_fixture_is_rejected"),
        platform: None,
    },
    Mutant {
        name: "mutant_config_shared_dir",
        issue: 18,
        breaks: "two identities may share a configuration directory or a database file",
        package: "bosun-config",
        check: Check::Test(
            "validate::tests::identities_sharing_a_directory_or_a_database_are_refused",
        ),
        platform: None,
    },
    Mutant {
        name: "mutant_config_skip_disk_probe",
        issue: 18,
        breaks: "the disk probe is ignored; a database may live on a network or synced folder",
        package: "bosun-config",
        check: Check::Test("validate::tests::a_database_on_a_network_or_synced_disk_is_refused"),
        platform: None,
    },
    Mutant {
        name: "mutant_store_commit_per_row",
        issue: 19,
        breaks: "the writer commits each row of a batch on its own",
        package: "bosun-store",
        check: Check::Test("writer::tests::an_abort_mid_batch_leaves_the_whole_batch_or_none"),
        platform: None,
    },
    Mutant {
        name: "mutant_store_accept_newer",
        issue: 19,
        breaks: "a database newer than the binary is not refused",
        package: "bosun-store",
        check: Check::Test("open::tests::a_newer_database_is_refused_untouched"),
        platform: None,
    },
    Mutant {
        name: "mutant_store_default_perms",
        issue: 19,
        breaks: "database files get the default mode instead of 0600",
        package: "bosun-store",
        check: Check::Test("open::tests::database_files_are_private"),
        // The mode bits exist on Unix only; Windows has no such test.
        platform: Some(Platform::Linux),
    },
    Mutant {
        name: "mutant_ingest_last_block",
        issue: 20,
        breaks: "the parser keeps the last usage block per file instead of every message",
        package: "bosun-ingest",
        check: Check::Test("claude::tests::every_fixture_equals_its_expected"),
        platform: None,
    },
    Mutant {
        name: "mutant_ingest_gap_keeps_line",
        issue: 20,
        breaks: "a gap's reason carries the line",
        package: "bosun-ingest",
        check: Check::Test("claude::tests::a_gap_holds_none_of_its_line"),
        platform: None,
    },
    Mutant {
        name: "mutant_ingest_abort_on_gap",
        issue: 20,
        breaks: "a gap aborts the rest of the file",
        package: "bosun-ingest",
        check: Check::Test("claude::tests::the_rest_of_a_file_parses_after_a_gap"),
        platform: None,
    },
    Mutant {
        name: "mutant_commit_time_only",
        issue: 23,
        breaks: "a commit is linked to sessions by time alone, whatever their repository",
        // The cfg is in bosun-store's link query; the check reads real repositories through
        // bosun-ingest's commit reader into the store, so it is bosun-ingest's test.
        package: "bosun-ingest",
        check: Check::Test("git::tests::each_commit_links_only_within_its_repository"),
        platform: None,
    },
    Mutant {
        name: "mutant_ingest_pass_files_summed",
        issue: 6,
        breaks: "a pass joined by a rescan counts each file once per walk",
        package: "bosun-ingest",
        check: Check::Test("tests::backfill::a_pass_joined_by_a_rescan_counts_each_file_once"),
        platform: None,
    },
    Mutant {
        name: "mutant_ingest_no_offset",
        issue: 21,
        breaks: "the tailer ignores the stored offset and re-reads every file",
        package: "bosun-ingest",
        check: Check::Test("tests::a_second_pass_parses_nothing"),
        platform: None,
    },
    Mutant {
        name: "mutant_ingest_no_dedup",
        issue: 21,
        breaks: "the message key includes the file id",
        package: "bosun-ingest",
        check: Check::Test("tests::the_fork_counts_once_in_either_scan_order"),
        platform: None,
    },
    Mutant {
        name: "mutant_ingest_parse_partial",
        issue: 21,
        breaks: "the tailer parses a last line that has no newline yet",
        package: "bosun-ingest",
        check: Check::Test("tests::a_partial_last_line_waits_for_its_newline"),
        platform: None,
    },
    Mutant {
        name: "mutant_ingest_reingest_truncates",
        issue: 21,
        breaks: "reingest deletes every row before it re-reads",
        package: "bosun-ingest",
        check: Check::Test("tests::reingest_updates_existing_files_and_keeps_deleted_ones"),
        platform: None,
    },
    Mutant {
        name: "mutant_ingest_cross_identity",
        issue: 21,
        breaks: "every identity discovers the first identity's directory",
        package: "bosun-ingest",
        check: Check::Test("tests::each_identity_holds_only_its_own_sessions"),
        platform: None,
    },
    Mutant {
        name: "mutant_ingest_skip_subagents",
        issue: 21,
        breaks: "discovery skips sub-agent transcripts",
        package: "bosun-ingest",
        check: Check::Test("tests::subagents_are_ingested_and_agree_with_cost_state"),
        platform: None,
    },
    Mutant {
        name: "mutant_api_skip_instance_check",
        issue: 24,
        breaks: "the daemon does not look for a running instance and removes a live socket",
        package: "bosund",
        check: Check::Test("a_second_instance_exits"),
        platform: None,
    },
    Mutant {
        name: "mutant_api_first_identity",
        issue: 24,
        breaks: "methods ignore the identity argument and serve the first identity",
        package: "bosund",
        check: Check::Test("tokens_never_cross_identities"),
        platform: None,
    },
    Mutant {
        name: "mutant_api_extra_field",
        issue: 24,
        breaks: "a protocol type gains a field without the schema being regenerated",
        package: "bosun-api",
        check: Check::Cargo(&["xtask", "gen-api", "--check"]),
        platform: None,
    },
    Mutant {
        name: "mutant_api_default_socket_mode",
        issue: 24,
        breaks: "the socket keeps the mode the umask gives it instead of 0600",
        package: "bosund",
        check: Check::Test("the_socket_is_private"),
        // The mode bits exist on Unix only; Windows has no such test.
        platform: Some(Platform::Linux),
    },
    Mutant {
        name: "mutant_ingest_backfill_first",
        issue: 22,
        breaks: "backfill work is taken before live tails",
        package: "bosun-ingest",
        check: Check::Test("tests::backfill::a_live_file_is_tailed_during_the_backfill"),
        platform: None,
    },
    Mutant {
        name: "mutant_ingest_backfill_from_zero",
        issue: 22,
        breaks: "a restarted ingest ignores the stored offsets and backfills from zero",
        package: "bosun-ingest",
        check: Check::Test(
            "tests::backfill::a_killed_backfill_resumes_without_parsing_a_byte_twice",
        ),
        platform: None,
    },
    Mutant {
        name: "mutant_ingest_unbounded",
        issue: 22,
        breaks: "the cap on open files is removed",
        package: "bosun-ingest",
        check: Check::Test("tests::backfill::the_backfill_holds_its_bounds"),
        platform: None,
    },
    Mutant {
        name: "mutant_cli_sum_identities",
        issue: 25,
        breaks: "the CLI sums every identity into one section",
        package: "bosun-cli",
        check: Check::Test("identities_are_separate_sections"),
        platform: None,
    },
    Mutant {
        name: "mutant_install_boot_trigger",
        issue: 28,
        breaks: "the Windows task starts at boot instead of at the user's logon",
        package: "bosun-cli",
        check: Check::Test("artefact::tests::each_artefact_names_a_logon_trigger"),
        platform: None,
    },
    Mutant {
        name: "mutant_doctor_skip_retention",
        issue: 28,
        breaks: "doctor skips the cleanupPeriodDays check against the reingest window",
        package: "bosun-cli",
        check: Check::Test("doctor_warns_when_cleanup_period_is_below_the_reingest_window"),
        platform: None,
    },
    Mutant {
        name: "mutant_report_main_only",
        issue: 26,
        breaks: "the cost-state agreement sums the main transcript only, not its sub-agents",
        package: "bosun-heat",
        check: Check::Test("exact::tests::the_agreement_counts_subagents"),
        platform: None,
    },
    Mutant {
        name: "mutant_report_no_carry",
        issue: 27,
        breaks: "a tool result is costed once, by its first write, with no cache carry",
        package: "bosun-heat",
        check: Check::Test("estimate::tests::a_tool_result_is_carried_until_the_compaction"),
        platform: None,
    },
    Mutant {
        name: "mutant_report_merge_estimates",
        issue: 27,
        breaks: "the estimated cost of tool results is folded into the exact total",
        package: "bosun-heat",
        check: Check::Test("estimate::tests::the_exact_total_holds_no_estimate"),
        platform: None,
    },
    Mutant {
        name: "mutant_report_no_window",
        issue: 46,
        breaks: "the cost-state agreement compares every message of a session, not its window",
        package: "bosun-heat",
        check: Check::Test("exact::tests::the_agreement_counts_the_provider_window_only"),
        platform: None,
    },
    Mutant {
        name: "mutant_ingest_subagent_cost_state",
        issue: 46,
        breaks: "a cost-state line in a sub-agent file replaces the root session's check",
        package: "bosun-ingest",
        check: Check::Test("tests::a_subagent_cost_state_is_not_the_session_check"),
        platform: None,
    },
    Mutant {
        name: "mutant_xtask_mcp_not_client",
        issue: 48,
        breaks: "bosun-mcp is dropped from the client list of the dependency rules",
        package: "xtask",
        check: Check::Test("deps::tests::mcp_is_a_client"),
        platform: None,
    },
    Mutant {
        name: "mutant_core_trust_done",
        issue: 49,
        breaks: "an exit text saying \"done\" skips the verdict",
        package: "bosun-core",
        check: Check::Test("item::tests::no_sequence_without_a_passing_verdict_reaches_done"),
        platform: None,
    },
    Mutant {
        name: "mutant_core_limit_is_failure",
        issue: 49,
        breaks: "a limit exit counts as a failed attempt",
        package: "bosun-core",
        check: Check::Test("item::tests::a_limit_exit_parks_and_resumes_uncounted"),
        platform: None,
    },
    Mutant {
        name: "mutant_core_no_park_cap",
        issue: 49,
        breaks: "uncounted parks and resumes have no per-item cap",
        package: "bosun-core",
        check: Check::Test("item::tests::fake_limits_in_a_row_end_in_needs_you"),
        platform: None,
    },
    Mutant {
        name: "mutant_core_flaky_is_fail",
        issue: 49,
        breaks: "a verdict that fails then passes is a failed attempt",
        package: "bosun-core",
        check: Check::Test("item::tests::fail_then_pass_is_flaky_and_done"),
        platform: None,
    },
    Mutant {
        name: "mutant_core_ignore_blockers",
        issue: 49,
        breaks: "the scheduler claims cards with an open blocked-by",
        package: "bosun-core",
        check: Check::Test("schedule::tests::order_is_board_order_skipping_open_blockers"),
        platform: None,
    },
    Mutant {
        name: "mutant_git_skip_clean_tree",
        issue: 53,
        breaks: "the clean-tree precondition passes every working tree",
        package: "bosun-git",
        check: Check::Test("tests::an_untracked_file_fails_clean_tree"),
        platform: None,
    },
    Mutant {
        name: "mutant_git_skip_ancestry",
        issue: 53,
        breaks: "the ancestry precondition passes a rewritten base and zero commits",
        package: "bosun-git",
        check: Check::Test("tests::a_reset_below_base_and_a_recommit_fails_ancestry"),
        platform: None,
    },
    Mutant {
        name: "mutant_git_skip_ref_check",
        issue: 53,
        breaks: "the refs precondition passes new, moved and deleted refs and a changed stash",
        package: "bosun-git",
        check: Check::Test("tests::a_new_tag_a_moved_branch_and_a_stash_entry_each_fail_refs"),
        platform: None,
    },
    Mutant {
        name: "mutant_git_skip_message_check",
        issue: 53,
        breaks: "the commit-message precondition passes closing keywords, skip-ci and forged trailers",
        package: "bosun-git",
        check: Check::Test(
            "tests::closing_keywords_skip_ci_and_forged_trailers_each_fail_messages",
        ),
        platform: None,
    },
    Mutant {
        name: "mutant_git_repo_hooks",
        issue: 53,
        breaks: "the invoker leaves the repository's hooks and fsmonitor live",
        package: "bosun-git",
        check: Check::Test("tests::planted_hooks_never_run_under_the_invoker"),
        platform: None,
    },
    Mutant {
        name: "mutant_proc_kill_parent_only",
        issue: 50,
        breaks: "kill_tree kills the direct child only, not its container",
        package: "bosun-proc",
        check: Check::Test("spawn::tests::kill_tree_kills_a_child_and_its_grandchild"),
        platform: None,
    },
    Mutant {
        name: "mutant_proc_skip_start_time",
        issue: 50,
        breaks: "reap kills a process by pid alone, whatever its start time",
        package: "bosun-proc",
        check: Check::Test("key::tests::reap_spares_a_process_whose_start_time_differs"),
        platform: None,
    },
    Mutant {
        name: "mutant_proc_no_kill_on_close",
        issue: 50,
        breaks: "the Job Object lacks kill-on-close; a dead holder's descendants live on",
        package: "bosun-proc",
        check: Check::Test("spawn::tests::a_dead_holder_leaves_no_descendant"),
        // Job Objects exist on Windows only.
        platform: Some(Platform::Windows),
    },
    Mutant {
        name: "mutant_runner_inline_daemon_comments",
        issue: 52,
        breaks: "the prompt builder inlines the daemon's own comments",
        package: "bosun-runner",
        check: Check::Test(
            "prompt::tests::daemon_comments_are_excluded_by_node_id_and_by_hidden_key",
        ),
        platform: None,
    },
    Mutant {
        name: "mutant_runner_auto_memory_on",
        issue: 135,
        breaks: "the Claude command leaves auto memory on",
        package: "bosun-runner",
        check: Check::Test("claude::tests::the_command_turns_auto_memory_off"),
        platform: None,
    },
    Mutant {
        name: "mutant_runner_limit_is_crash",
        issue: 52,
        breaks: "a usage-limit exit classifies as a crash",
        package: "bosun-runner",
        check: Check::Test("claude::tests::a_limit_yields_its_reset_or_none"),
        platform: None,
    },
    Mutant {
        name: "mutant_runner_loose_mcp",
        issue: 52,
        breaks: "the Claude command drops --strict-mcp-config",
        package: "bosun-runner",
        check: Check::Test("claude::tests::the_command_loads_only_the_session_mcp_servers"),
        platform: None,
    },
    Mutant {
        name: "mutant_runner_question_from_text",
        issue: 52,
        breaks: "a question in the agent's final text classifies the exit as a question",
        package: "bosun-runner",
        check: Check::Test("claude::tests::a_question_in_the_text_is_not_a_question"),
        platform: None,
    },
    Mutant {
        name: "mutant_ingest_exec_is_read",
        issue: 76,
        breaks: "a script run by an interpreter is recorded as a read of it",
        package: "bosun-ingest",
        check: Check::Test("tests::twelve_runs_of_a_script_are_execs_not_reads"),
        platform: None,
    },
    Mutant {
        name: "mutant_ingest_unknown_is_read",
        issue: 76,
        breaks: "a file named by an unknown command is recorded as a read of it",
        package: "bosun-ingest",
        check: Check::Test("shell::tests::an_unknown_verb_names_an_unknown_touch"),
        platform: None,
    },
    Mutant {
        name: "mutant_queue_lint_off",
        issue: 51,
        breaks: "card lint finds nothing, so a card with no verdict is claimable",
        package: "bosun-queue",
        check: Check::Test("lint::tests::a_card_with_no_verdict_is_not_claimable"),
        platform: None,
    },
    Mutant {
        name: "mutant_queue_lint_functions_are_programs",
        issue: 120,
        breaks: "a function the verdict defines is taken for a program, so the verdict is refused",
        package: "bosun-queue",
        check: Check::Test("lint::tests::a_function_the_verdict_defines_is_not_a_missing_program"),
        platform: None,
    },
    Mutant {
        name: "mutant_queue_local_no_key",
        issue: 51,
        breaks: "the local-file backend does not look comments up by key before posting",
        package: "bosun-queue",
        check: Check::Test("local::tests::suite_comment_replayed_with_one_key_leaves_one_comment"),
        platform: None,
    },
    Mutant {
        name: "mutant_queue_stale_poll",
        issue: 51,
        breaks: "the local-file poll trusts an unchanged file length, so a reorder is not seen",
        package: "bosun-queue",
        check: Check::Test("local::tests::suite_a_reorder_alone_appears_in_the_next_poll"),
        platform: None,
    },
    Mutant {
        name: "mutant_identity_inherit_env",
        issue: 54,
        breaks: "the constructed environment is the daemon's inherited one",
        package: "bosun-identity",
        check: Check::Test(
            "tests::a_sentinel_credential_in_the_daemon_environment_is_absent_in_the_child",
        ),
        platform: None,
    },
    Mutant {
        name: "mutant_identity_skip_login",
        issue: 54,
        breaks: "the token's login is not compared with the expected login",
        package: "bosun-identity",
        check: Check::Test("tests::wrong_login_is_refused"),
        platform: None,
    },
    Mutant {
        name: "mutant_identity_agent_gets_bundle",
        issue: 54,
        breaks: "the agent environment is the daemon's tool environment, the full bundle",
        package: "bosun-identity",
        check: Check::Test("tests::the_agent_environment_has_no_github_credential_of_the_identity"),
        platform: None,
    },
    Mutant {
        name: "mutant_identity_skip_allowlist",
        issue: 54,
        breaks: "the remote's owner is not compared with the identity's allowlist",
        package: "bosun-identity",
        check: Check::Test("tests::remote_owner_off_the_allowlist_is_refused"),
        platform: None,
    },
    Mutant {
        name: "mutant_identity_skip_author",
        issue: 54,
        breaks: "the resolved git author is not compared with the configured one",
        package: "bosun-identity",
        check: Check::Test("tests::wrong_author_is_refused"),
        platform: None,
    },
    Mutant {
        name: "mutant_identity_declared_credentials",
        issue: 54,
        breaks: "a repository may declare a credential variable",
        package: "bosun-identity",
        check: Check::Test("tests::a_repository_asking_for_gh_token_does_not_get_it"),
        platform: None,
    },
    Mutant {
        name: "mutant_store_transition_no_key",
        issue: 57,
        breaks: "a replayed transition is not recognised by its idempotence key",
        package: "bosun-store",
        check: Check::Test("work::tests::a_transition_written_twice_is_one_row"),
        platform: None,
    },
    Mutant {
        name: "mutant_store_outbox_no_coalesce",
        issue: 57,
        breaks: "pending upsert_comment rows of one key do not coalesce",
        package: "bosun-store",
        check: Check::Test("work::tests::report_writes_of_one_key_coalesce_into_one_pending_row"),
        platform: None,
    },
    Mutant {
        name: "mutant_store_run_kind_order",
        issue: 57,
        breaks: "a session is marked run only when its run row is written first",
        package: "bosun-store",
        check: Check::Test("work::tests::a_transcript_is_a_run_session_in_either_order"),
        platform: None,
    },
    Mutant {
        name: "mutant_api_no_event_seq",
        issue: 69,
        breaks: "a subscription ignores the client's sequence: a reconnect starts from now",
        package: "bosund",
        check: Check::Test("stream_tests::a_reconnect_gets_every_missed_event_in_order"),
        platform: None,
    },
    Mutant {
        name: "mutant_api_slow_subscriber_blocks",
        issue: 69,
        breaks: "the publisher waits for a subscriber that has fallen behind",
        package: "bosund",
        check: Check::Test(
            "stream_tests::a_subscriber_that_stops_reading_is_dropped_and_never_slows_the_publisher",
        ),
        platform: None,
    },
    Mutant {
        name: "mutant_api_tail_always_on",
        issue: 69,
        breaks: "session output lines reach subscriptions that did not opt into the tail",
        package: "bosund",
        check: Check::Test("stream_tests::output_lines_reach_only_subscriptions_that_opted_in"),
        platform: None,
    },
    Mutant {
        name: "mutant_config_reload_adopts_invalid",
        issue: 55,
        breaks: "a refused configuration edit replaces the configuration in force",
        package: "bosun-config",
        check: Check::Test("tests::a_broken_edit_keeps_the_previous_configuration"),
        platform: None,
    },
    Mutant {
        name: "mutant_config_hash_ignores_policy",
        issue: 55,
        breaks: "the policy and configuration hashes leave the protected paths out",
        package: "bosun-config",
        check: Check::Test("tests::the_policy_hash_follows_protected_paths_only"),
        platform: None,
    },
    Mutant {
        name: "mutant_config_hash_always_extended",
        issue: 182,
        breaks: "the keys added by #149 are hashed at their defaults, so a stage-1 repository's hash changes",
        package: "bosun-config",
        check: Check::Test("tests::a_stage_1_repository_keeps_its_hash"),
        platform: Some(Platform::Windows),
    },
    Mutant {
        name: "mutant_config_gitignore_unprotected",
        issue: 55,
        breaks: "`.gitignore` is protected only when the policy lists it",
        package: "bosun-config",
        check: Check::Test("tests::a_policy_that_omits_gitignore_still_protects_it"),
        platform: None,
    },
    Mutant {
        name: "mutant_queue_no_author_filter",
        issue: 60,
        breaks: "comments and bodies from logins off the author allowlist reach the card",
        package: "bosun-queue",
        check: Check::Test(
            "projects_v2::tests::a_comment_from_a_login_off_the_allowlist_is_absent_and_counted",
        ),
        platform: None,
    },
    Mutant {
        name: "mutant_queue_ambiguous_ok",
        issue: 60,
        breaks: "a Status option matched by two columns' names is accepted at start",
        package: "bosun-queue",
        check: Check::Test("projects_v2::tests::a_duplicated_status_option_refuses_at_start"),
        platform: None,
    },
    Mutant {
        name: "mutant_queue_no_cursor",
        issue: 60,
        breaks: "every poll fetches every issue again instead of those whose updatedAt moved",
        package: "bosun-queue",
        check: Check::Test("projects_v2::tests::an_unchanged_board_costs_one_request_per_poll"),
        platform: None,
    },
    Mutant {
        name: "mutant_publish_passthrough",
        issue: 56,
        breaks: "the sanitiser passes agent-influenced Markdown through unchanged (#8)",
        package: "bosun-core",
        check: Check::Test("publish::sanitise::tests::the_hostile_corpus_comes_out_inert"),
        platform: None,
    },
    Mutant {
        name: "mutant_publish_no_redact",
        issue: 56,
        breaks: "the secret redactor returns its input unchanged",
        package: "bosun-core",
        check: Check::Test("publish::report::tests::a_token_in_a_verdict_tail_is_redacted"),
        platform: None,
    },
    Mutant {
        name: "mutant_publish_truncate_back",
        issue: 56,
        breaks: "an over-long verdict tail keeps its first lines instead of its last",
        package: "bosun-core",
        check: Check::Test(
            "publish::report::tests::a_200_kb_tail_renders_under_the_cap_and_keeps_its_last_lines",
        ),
        platform: None,
    },
    Mutant {
        name: "mutant_publish_short_fence",
        issue: 56,
        breaks: "code fences have a fixed length instead of outrunning the content's backticks",
        package: "bosun-core",
        check: Check::Test("publish::report::tests::ten_backticks_cannot_close_the_fence"),
        platform: None,
    },
    Mutant {
        name: "mutant_api_default_pipe_acl",
        issue: 71,
        breaks: "the named pipe keeps the default DACL, which admits more than its owner",
        package: "bosund",
        check: Check::Test("listen::windows::tests::every_instance_admits_only_the_owner"),
        platform: Some(Platform::Windows),
    },
    Mutant {
        name: "mutant_doctor_skip_tool_paths",
        issue: 71,
        breaks: "doctor does not look for sh, the agent CLI and gh on the daemon's PATH",
        package: "bosun-cli",
        check: Check::Test("doctor_fails_naming_a_missing_tool"),
        platform: None,
    },
    Mutant {
        name: "mutant_report_item_last_attempt",
        issue: 72,
        breaks: "an item's runs bill only its last attempt",
        package: "bosun-heat",
        check: Check::Test("items::tests::an_items_sums_are_its_run_sessions_messages"),
        platform: None,
    },
    Mutant {
        name: "mutant_report_merge_inferred",
        issue: 72,
        breaks: "the inferred sessions of an item are added to its exact runs",
        package: "bosun-heat",
        check: Check::Test("items::tests::an_inferred_session_is_an_estimate_only"),
        platform: None,
    },
    Mutant {
        name: "mutant_report_commit_by_time",
        issue: 72,
        breaks: "a commit is linked to the run whose time span contains it, not by its range",
        package: "bosun-heat",
        check: Check::Test("items::tests::every_commit_of_the_runs_maps_to_one_session_and_item"),
        platform: None,
    },
    Mutant {
        name: "mutant_runner_merge_streams",
        issue: 58,
        breaks: "the verdict line is the interleaved tail's last line, not stdout's",
        package: "bosun-runner",
        check: Check::Test(
            "verdict::tests::a_stderr_line_after_stdouts_match_reports_the_stdout_line",
        ),
        platform: None,
    },
    Mutant {
        name: "mutant_runner_no_flaky_names",
        issue: 58,
        breaks: "the libtest failure parser returns no names",
        package: "bosun-runner",
        check: Check::Test(
            "verdict::tests::libtest_output_with_three_failures_yields_the_three_names",
        ),
        platform: None,
    },
    Mutant {
        name: "mutant_runner_no_verdict_timeout",
        issue: 58,
        breaks: "the verdict timeout never fires, so a hanging verdict holds the attempt",
        package: "bosun-runner",
        check: Check::Test(
            "verdict::tests::a_verdict_past_its_timeout_is_timeout_and_no_descendant_survives",
        ),
        platform: None,
    },
    Mutant {
        name: "mutant_queue_skip_claim_reread",
        issue: 61,
        breaks: "the claim is taken for granted after the Status write, with no re-read (#5)",
        package: "bosun-queue",
        check: Check::Test(
            "projects_v2::tests::a_user_move_between_the_claim_write_and_the_re_read_releases_the_claim",
        ),
        platform: None,
    },
    Mutant {
        name: "mutant_queue_no_comment_key",
        issue: 61,
        breaks: "the Projects v2 backend does not look comments up by key before posting (#5)",
        package: "bosun-queue",
        check: Check::Test(
            "projects_v2::tests::a_comment_whose_response_was_lost_is_found_by_key_on_replay",
        ),
        platform: None,
    },
    Mutant {
        name: "mutant_queue_recreate_always",
        issue: 61,
        breaks: "a sticky comment the user deleted again is recreated again, without end",
        package: "bosun-queue",
        check: Check::Test("projects_v2::tests::suite_a_sticky_deleted_twice_is_recreated_once"),
        platform: None,
    },
    Mutant {
        name: "mutant_git_ignored_is_warning",
        issue: 59,
        breaks: "a new ignored file outside the build directories is a warning, not Needs you",
        package: "bosun-git",
        check: Check::Test(
            "tests::a_hidden_file_behind_a_new_gitignore_line_is_refused_but_not_in_a_build_dir",
        ),
        platform: None,
    },
    Mutant {
        name: "mutant_git_skip_protected_paths",
        issue: 59,
        breaks: "commits touching protected paths reach the verdict",
        package: "bosun-git",
        check: Check::Test(
            "tests::a_commit_touching_the_verdicts_test_is_refused_with_the_diff_stat",
        ),
        platform: None,
    },
    Mutant {
        name: "mutant_git_skip_config_snapshot",
        issue: 59,
        breaks: "a planted hook or an edit of git's configuration reaches the verdict",
        package: "bosun-git",
        check: Check::Test(
            "tests::a_planted_hook_an_include_edit_and_an_exclude_edit_are_each_refused",
        ),
        platform: None,
    },
    Mutant {
        name: "mutant_git_trailer_all_commits",
        issue: 59,
        breaks: "the injected hook adds the trailers without the injection marker",
        package: "bosun-git",
        check: Check::Test(
            "tests::a_commit_without_the_injection_carries_none_and_its_hooks_run_as_before",
        ),
        platform: None,
    },
    Mutant {
        name: "mutant_ingest_no_session_span",
        issue: 95,
        breaks: "the store's writer never sets a window's span from its messages",
        package: "bosun-ingest",
        check: Check::Test("tests::an_ingested_session_links_a_commit_inside_its_span"),
        platform: None,
    },
    Mutant {
        name: "mutant_loop_skip_base_verdict",
        issue: 62,
        breaks: "the claim skips the verdict on the base commit (D7): a card that passes there is spawned",
        package: "bosund",
        check: Check::Test(SCENARIO_62),
        platform: None,
    },
    Mutant {
        name: "mutant_loop_live_verdict",
        issue: 62,
        breaks: "the verdict runs the live card's command, not the snapshot's (#5)",
        package: "bosund",
        check: Check::Test(SCENARIO_62),
        platform: None,
    },
    Mutant {
        name: "mutant_loop_effect_before_transition",
        issue: 62,
        breaks: "the loop performs an arrow's effects before its row is written",
        package: "bosund",
        check: Check::Test(
            "executor::tests::a_refused_transition_leaves_no_effect_without_its_row",
        ),
        platform: None,
    },
    Mutant {
        name: "mutant_loop_ignore_hash_change",
        issue: 62,
        breaks: "a configuration or policy change during a session goes unnoticed",
        package: "bosund",
        check: Check::Test(SCENARIO_62),
        platform: None,
    },
    // The "reaches Done" forms of #53's mutants, in the loop's scenario suite (#62).
    Mutant {
        name: "mutant_git_skip_clean_tree",
        issue: 62,
        breaks: "an item that passes only because of an uncommitted file reaches Done",
        package: "bosund",
        check: Check::Test(SCENARIO_62),
        platform: None,
    },
    Mutant {
        name: "mutant_git_skip_ancestry",
        issue: 62,
        breaks: "an agent that resets below the base reaches the verdict and Done",
        package: "bosund",
        check: Check::Test(SCENARIO_62),
        platform: None,
    },
    // #63: the agent.* methods, the per-session credential, rate caps.
    Mutant {
        name: "mutant_agent_any_item",
        issue: 63,
        breaks: "an agent credential admits a call naming another item",
        package: "bosund",
        check: Check::Test(SCENARIO_63),
        platform: None,
    },
    Mutant {
        name: "mutant_agent_no_rate_cap",
        issue: 63,
        breaks: "an agent tool has no per-attempt cap",
        package: "bosund",
        check: Check::Test(SCENARIO_63),
        platform: None,
    },
    Mutant {
        name: "mutant_agent_first_summary_wins",
        issue: 63,
        breaks: "the first summary of an attempt wins, not the last",
        package: "bosun-store",
        check: Check::Test("work::tests::the_last_accepted_summary_of_an_attempt_wins"),
        platform: None,
    },
    // Parking in the loop (#66).
    Mutant {
        name: "mutant_loop_limit_counts",
        issue: 66,
        breaks: "the loop reports a limit as a crash: a second limit fails the attempt",
        package: "bosund",
        check: Check::Test("executor::tests::a_limit_parks_and_resumes_without_counting"),
        platform: None,
    },
    Mutant {
        name: "mutant_loop_no_park_cap",
        issue: 66,
        breaks: "the loop does not apply `daemon.park_cap`: a fake limit forever parks forever",
        package: "bosund",
        check: Check::Test("executor::tests::a_fake_limit_forever_ends_in_needs_you_at_the_cap"),
        platform: None,
    },
    Mutant {
        name: "mutant_loop_auth_retries",
        issue: 66,
        breaks: "an expired authentication is retried like a crash",
        package: "bosund",
        check: Check::Test("executor::tests::an_expired_authentication_is_needs_you_with_no_retry"),
        platform: None,
    },
    Mutant {
        name: "mutant_loop_skip_wake_check",
        issue: 66,
        breaks: "after a sleep the session's child is not revalidated: a dead one is waited on",
        package: "bosund",
        check: Check::Test(
            "executor::tests::after_a_sleep_a_dead_child_is_retried_once_uncounted_and_a_live_one_runs_on",
        ),
        platform: None,
    },
    Mutant {
        name: "mutant_mcp_skip_handshake",
        issue: 64,
        breaks: "a session whose bosun-mcp never says hello runs on without its tools",
        package: "bosund",
        check: Check::Test(
            "executor::tests::a_session_whose_mcp_server_cannot_start_is_retried_once_then_needs_you",
        ),
        platform: None,
    },
    Mutant {
        name: "mutant_mcp_extra_tool",
        issue: 64,
        breaks: "bosun-mcp lists a `done` tool",
        package: "bosun-mcp",
        check: Check::Test("server::tests::tools_list_is_exactly_the_three_tools"),
        platform: None,
    },
    Mutant {
        name: "mutant_mcp_config_in_tree",
        issue: 64,
        breaks: "the per-session MCP configuration is written into the working tree",
        package: "bosund",
        check: Check::Test(
            "executor::tests::a_tool_call_reaches_the_store_through_bosun_mcp_and_the_configuration_stays_private",
        ),
        platform: None,
    },
    Mutant {
        name: "mutant_control_pause_kills",
        issue: 67,
        breaks: "a pause kills the session like the kill switch: the attempt in flight never finishes",
        package: "bosund",
        check: Check::Test(
            "executor::tests::control::a_pause_lets_the_attempt_finish_and_starts_nothing_until_resume",
        ),
        platform: None,
    },
    Mutant {
        name: "mutant_control_kill_keeps_writing",
        issue: 67,
        breaks: "the kill switch leaves board writes going: the claim and the outbox write on",
        package: "bosund",
        check: Check::Test(
            "executor::tests::control::the_kill_switch_leaves_no_child_and_writes_no_board",
        ),
        platform: None,
    },
    Mutant {
        name: "mutant_budget_ignore_item_cap",
        issue: 67,
        breaks: "an item over its eqv or wall-clock cap runs on",
        package: "bosund",
        check: Check::Test(
            "executor::tests::control::an_item_over_its_eqv_or_wall_clock_cap_is_killed_and_needs_you",
        ),
        platform: None,
    },
    Mutant {
        name: "mutant_control_autopause_own_sessions",
        issue: 67,
        breaks: "a session bosun spawned pauses its own repository as if it were interactive",
        package: "bosund",
        check: Check::Test(
            "executor::tests::control::an_interactive_session_pauses_its_repository_and_a_run_session_does_not",
        ),
        platform: None,
    },
    Mutant {
        name: "mutant_control_no_operator_token",
        issue: 67,
        breaks: "the control switches skip the operator token: an agent's own connection pauses the daemon",
        package: "bosund",
        check: Check::Test(
            "executor::tests::control::a_pause_lets_the_attempt_finish_and_starts_nothing_until_resume",
        ),
        platform: None,
    },
    Mutant {
        name: "mutant_control_keep_operator_token",
        issue: 117,
        breaks: "a daemon stopped cleanly leaves its operator token file behind",
        package: "bosund",
        check: Check::Test("control::tests::a_clean_stop_removes_only_its_own_token"),
        platform: None,
    },
    // #99: the guards of #59 in the loop; the "reaches Done" forms of #59's mutants.
    Mutant {
        name: "mutant_loop_skip_guards",
        issue: 99,
        breaks: "the loop skips the guards of #59: the protected-path item reaches Done",
        package: "bosund",
        check: Check::Test(SCENARIO_99),
        platform: None,
    },
    Mutant {
        name: "mutant_git_ignored_is_warning",
        issue: 99,
        breaks: "an item that leaves a new ignored file its verdict needs reaches Done",
        package: "bosund",
        check: Check::Test(SCENARIO_99),
        platform: None,
    },
    Mutant {
        name: "mutant_git_skip_protected_paths",
        issue: 99,
        breaks: "an item whose commits touch a protected path reaches Done",
        package: "bosund",
        check: Check::Test(SCENARIO_99),
        platform: None,
    },
    Mutant {
        name: "mutant_git_skip_config_snapshot",
        issue: 99,
        breaks: "an item that plants a hook reaches Done",
        package: "bosund",
        check: Check::Test(SCENARIO_99),
        platform: None,
    },
    Mutant {
        name: "mutant_git_trailer_all_commits",
        issue: 99,
        breaks: "a run's commit made with the trailer marker off gets the trailers and leaves the report",
        package: "bosund",
        check: Check::Test(SCENARIO_99),
        platform: None,
    },
    Mutant {
        name: "mutant_cli_action_without_command",
        issue: 70,
        breaks: "an operator action of the shared table (restart-item) has no CLI command",
        package: "bosun-cli",
        check: Check::Test("tests::every_action_has_a_command"),
        platform: None,
    },
    Mutant {
        name: "mutant_api_queue_other_identity",
        issue: 70,
        breaks: "queue.get and queue.lint read the repositories of every identity, not only the one asked for",
        package: "bosund",
        check: Check::Test("view_tests::a_queue_holds_only_the_identitys_repositories"),
        platform: None,
    },
    Mutant {
        name: "mutant_outbox_no_key",
        issue: 65,
        breaks: "board comments are sent without their idempotence key: a replay duplicates them",
        package: "bosund",
        check: Check::Test(
            "executor::tests::sticky::a_ten_minute_outage_mid_item_ends_the_item_and_the_board_converges_once",
        ),
        platform: None,
    },
    Mutant {
        name: "mutant_outbox_stored_body",
        issue: 65,
        breaks: "the report body first rendered for an outbox row is replayed as stored",
        package: "bosund",
        check: Check::Test(
            "executor::tests::sticky::an_out_of_order_replay_leaves_the_newest_report",
        ),
        platform: None,
    },
    Mutant {
        name: "mutant_outbox_stop_on_outage",
        issue: 65,
        breaks: "landing waits for the board: an outage stops the item",
        package: "bosund",
        check: Check::Test(
            "executor::tests::sticky::a_ten_minute_outage_mid_item_ends_the_item_and_the_board_converges_once",
        ),
        platform: None,
    },
    Mutant {
        name: "mutant_doctor_skip_stop_hook",
        issue: 75,
        breaks: "doctor does not look for a Stop hook left in a configured repository's agent settings",
        package: "bosun-cli",
        check: Check::Test("doctor_reports_a_leftover_stop_hook_by_path"),
        platform: None,
    },
    Mutant {
        name: "mutant_tui_no_confirm",
        issue: 73,
        breaks: "an action key of the TUI sends its action at once, without its confirmation",
        package: "bosun-tui",
        check: Check::Test("feed::tests::a_key_without_its_confirmation_sends_nothing"),
        platform: None,
    },
    Mutant {
        name: "mutant_tui_no_reconnect",
        issue: 73,
        breaks: "the TUI never opens its request connection again once it broke",
        package: "bosun-tui",
        check: Check::Test("feed::tests::the_view_survives_a_daemon_restart"),
        platform: None,
    },
    Mutant {
        name: "mutant_queue_markers_through_allowlist",
        issue: 106,
        breaks: "the identity's own markers go through the author allowlist: a second daemon is not seen",
        package: "bosun-queue",
        check: Check::Test(
            "projects_v2::tests::a_second_daemons_marker_is_seen_with_the_login_off_the_allowlist",
        ),
        platform: None,
    },
    Mutant {
        name: "mutant_queue_markers_through_allowlist",
        issue: 106,
        breaks: "with the login off the allowlist, the loop claims a card another daemon marked",
        package: "bosund",
        check: Check::Test(
            "executor::tests::board_scenes::a_second_daemon_is_refused_with_the_login_off_the_allowlist",
        ),
        platform: None,
    },
    Mutant {
        name: "mutant_loop_no_midrun_poll",
        issue: 106,
        breaks: "the board is not read while an item is in flight: an item whose issue is deleted runs on",
        package: "bosund",
        check: Check::Test(
            "executor::tests::board_scenes::an_issue_deleted_mid_session_cancels_the_item_and_kills_the_session",
        ),
        platform: None,
    },
    Mutant {
        name: "mutant_loop_no_idle_backoff",
        issue: 106,
        breaks: "an idle Projects v2 board is read at every tick",
        package: "bosund",
        check: Check::Test(
            "executor::board::tests::an_idle_board_is_read_less_often_than_an_active_one",
        ),
        platform: None,
    },
    Mutant {
        name: "mutant_loop_ignores_quota",
        issue: 106,
        breaks: "a Projects v2 board with less than 10% of its quota left is read at the usual pace",
        package: "bosund",
        check: Check::Test("executor::board::tests::a_low_quota_waits_for_the_reset"),
        platform: None,
    },
    Mutant {
        name: "mutant_recover_skip_reap",
        issue: 68,
        breaks: "recovery does not kill the children a previous daemon left running",
        package: "bosund",
        check: Check::Test(
            "executor::tests::recovery::a_daemon_killed_mid_session_resumes_once_and_leaves_no_child",
        ),
        platform: None,
    },
    Mutant {
        name: "mutant_recover_counts_crash",
        issue: 68,
        breaks: "the work lost with the daemon counts: a lost verdict run is a run that did not pass",
        package: "bosund",
        check: Check::Test(
            "executor::tests::recovery::a_daemon_killed_mid_verdict_runs_it_again_uncounted",
        ),
        platform: None,
    },
    Mutant {
        name: "mutant_recover_resume_before_board",
        issue: 68,
        breaks: "recovery resumes the lost session before it reads the user's cancel on the board",
        package: "bosund",
        check: Check::Test(
            "executor::tests::recovery::a_cancel_made_while_the_daemon_was_down_wins_over_the_resume",
        ),
        platform: None,
    },
    Mutant {
        name: "mutant_outbox_no_key",
        issue: 68,
        breaks: "a board write repeated after a daemon died before its acknowledgement duplicates",
        package: "bosund",
        check: Check::Test(
            "executor::tests::recovery::a_daemon_killed_between_a_board_write_and_its_acknowledgement_leaves_one_write",
        ),
        platform: None,
    },
    Mutant {
        name: "mutant_report_tails_on_public",
        issue: 111,
        breaks: "the visibility is not asked: a public repository's sticky report carries verdict output",
        package: "bosund",
        check: Check::Test(
            "executor::tests::board_scenes::the_verdict_tail_is_published_on_a_private_repository_only",
        ),
        platform: None,
    },
    Mutant {
        name: "mutant_queue_closed_not_cancel",
        issue: 111,
        breaks: "a closed issue is not a cancel: the item in flight runs on",
        package: "bosund",
        check: Check::Test(
            "executor::tests::board_scenes::an_issue_closed_mid_session_cancels_the_item_and_kills_the_session",
        ),
        platform: None,
    },
    Mutant {
        name: "mutant_tui_ignore_resync",
        issue: 74,
        breaks: "the Session tab ignores a resync of its tail and keeps the lines it has, with a gap",
        package: "bosun-tui",
        check: Check::Test("feed::tests::after_a_resync_the_tail_is_refetched_with_no_gap"),
        platform: None,
    },
    Mutant {
        name: "mutant_tui_pace_unlabelled",
        issue: 74,
        breaks: "the Tokens tab shows the reference pace without saying it is configured",
        package: "bosun-tui",
        check: Check::Test("view::tests::the_speedometer_labels_its_reference_configured"),
        platform: None,
    },
    Mutant {
        name: "mutant_runner_no_default_model",
        issue: 133,
        breaks: "a card without `model` spawns its session with no `--model`: the repository's default is not passed",
        package: "bosund",
        check: Check::Test(
            "executor::tests::a_session_asks_for_the_card_model_else_the_repository_default",
        ),
        platform: None,
    },
    Mutant {
        name: "mutant_config_queue_without_model",
        issue: 133,
        breaks: "a repository with a queue and no `model` is accepted at load",
        package: "bosun-config",
        check: Check::Test("tests::a_repository_with_a_queue_and_no_model_is_refused"),
        platform: None,
    },
    Mutant {
        name: "mutant_lease_one_by_one",
        issue: 121,
        breaks: "leases are taken one by one in the order a request lists them, not by name",
        package: "bosun-core",
        check: Check::Test("lease::tests::two_items_wanting_the_same_pair_never_deadlock"),
        platform: None,
    },
    Mutant {
        name: "mutant_lease_keep_dead_holder",
        issue: 121,
        breaks: "release_stale keeps the leases of a holder that is gone",
        package: "bosun-core",
        check: Check::Test("lease::tests::a_lease_whose_holder_is_gone_is_released"),
        platform: None,
    },
    Mutant {
        name: "mutant_lease_hold_and_wait",
        issue: 121,
        breaks: "an attempt that finds a resource held keeps what it took while it waits",
        package: "bosun-core",
        check: Check::Test(
            "lease::tests::an_attempt_that_finds_one_resource_held_takes_none_of_the_set",
        ),
        platform: None,
    },
    Mutant {
        name: "mutant_publish_pr_body_overwrite",
        issue: 122,
        breaks: "updating a pull request body replaces all of it, not only the daemon's region",
        package: "bosun-core",
        check: Check::Test("publish::pr::tests::an_update_rewrites_only_the_daemon_region"),
        platform: None,
    },
    Mutant {
        name: "mutant_publish_pr_raw_summary",
        issue: 122,
        breaks: "the agent's summary goes into the pull request body unsanitised",
        package: "bosun-core",
        check: Check::Test(
            "publish::pr::tests::the_hostile_corpus_stays_inert_in_a_pull_request_body",
        ),
        platform: None,
    },
    Mutant {
        name: "mutant_publish_redact_before_clean",
        issue: 138,
        breaks: "the report redacts text before cleaning it, so a secret split by an invisible character is published",
        package: "bosun-core",
        check: Check::Test(
            "publish::report::tests::a_secret_split_by_an_invisible_character_is_redacted",
        ),
        platform: None,
    },
    Mutant {
        name: "mutant_config_resource_any_capacity",
        issue: 123,
        breaks: "a resource with a capacity of 0, which no one can ever hold, is accepted",
        package: "bosun-config",
        check: Check::Test("tests::a_resource_needs_a_positive_capacity_and_a_known_scope"),
        platform: None,
    },
    Mutant {
        name: "mutant_config_worktree_root_inside_repo",
        issue: 123,
        breaks: "a worktree root inside the repository's own checkout is accepted",
        package: "bosun-config",
        check: Check::Test("tests::a_worktree_root_inside_the_repository_is_refused"),
        platform: None,
    },
    // #127: free disk space.
    Mutant {
        name: "mutant_proc_free_space_missing_is_unlimited",
        issue: 127,
        breaks: "a missing path reports unlimited free space",
        package: "bosun-proc",
        check: Check::Test(
            "disk::tests::free_space_is_reported_for_a_directory_and_refused_for_a_missing_one",
        ),
        platform: None,
    },
    // #128: escape attempts and the members of a container.
    Mutant {
        name: "mutant_proc_allow_breakaway",
        issue: 128,
        breaks: "the Job Object allows breakaway; a grandchild leaves it and outlives the tree",
        package: "bosun-proc",
        check: Check::Test("spawn::tests::a_grandchild_that_tries_to_escape_dies_with_the_tree"),
        // Job Objects exist on Windows only.
        platform: Some(Platform::Windows),
    },
    Mutant {
        name: "mutant_proc_members_child_only",
        issue: 128,
        breaks: "members lists the direct child only, not the descendants left in the container",
        package: "bosun-proc",
        check: Check::Test("spawn::tests::members_lists_the_processes_left_in_the_container"),
        platform: None,
    },
    // #129: the session shim.
    Mutant {
        name: "mutant_shim_no_exit_record",
        issue: 129,
        breaks: "the shim never writes the child's exit into its record",
        package: "bosun-proc",
        check: Check::Test("the_exit_of_a_child_nobody_waited_for_is_in_the_record"),
        platform: None,
    },
    // #130: the shim spawner and re-attach.
    Mutant {
        name: "mutant_shim_attach_by_pid",
        issue: 130,
        breaks: "attach takes a live pid for the shim whatever its start time",
        package: "bosun-proc",
        check: Check::Test("attach_refuses_a_record_whose_pid_changed_hands"),
        platform: None,
    },
    Mutant {
        name: "mutant_shim_child_shares_the_group",
        issue: 130,
        breaks: "the shim's child stays in the shim's process group; a kill misses its descendants",
        package: "bosun-proc",
        check: Check::Test("a_session_lists_its_container_and_kill_tree_ends_it"),
        // Process groups are Unix; the test reads the groups from `/proc`.
        platform: Some(Platform::Linux),
    },
    // #148: the prompt through a stdin file of the shim.
    Mutant {
        name: "mutant_shim_stdin_closed",
        issue: 148,
        breaks: "the shim ignores the spec's stdin file and gives the child a closed stdin",
        package: "bosun-proc",
        check: Check::Test("a_child_reads_its_prompt_from_the_stdin_file"),
        platform: None,
    },
    Mutant {
        name: "mutant_shim_accept_old_spec",
        issue: 148,
        breaks: "the version of a spec is not compared; an older spec is run",
        package: "bosun-proc",
        check: Check::Test("a_version_1_spec_is_refused_by_name"),
        platform: None,
    },
    // #183: the prompt and the spec owner-only, and no prompt left behind.
    Mutant {
        name: "mutant_shim_stdin_default_permissions",
        issue: 183,
        breaks: "the stdin file and the spec get default permissions; other users may read them",
        package: "bosun-proc",
        check: Check::Test("the_stdin_and_spec_files_admit_only_the_user"),
        platform: None,
    },
    Mutant {
        name: "mutant_shim_prompt_left_on_error",
        issue: 183,
        breaks: "a spawn that fails after writing the prompt leaves it on disk",
        package: "bosun-proc",
        check: Check::Test("a_failed_spawn_leaves_no_prompt"),
        platform: None,
    },
    // #124: item worktrees.
    Mutant {
        name: "mutant_git_worktree_skip_disk_floor",
        issue: 124,
        breaks: "a worktree is made below the disk floor",
        package: "bosun-git",
        check: Check::Test("worktree::tests::no_worktree_is_created_below_the_disk_floor"),
        platform: None,
    },
    Mutant {
        name: "mutant_git_worktree_unremoved_is_removed",
        issue: 124,
        breaks: "a worktree that could not be removed is reported as removed",
        package: "bosun-git",
        check: Check::Test(
            "worktree::tests::a_worktree_with_a_file_held_open_is_quarantined_and_reported",
        ),
        // Only Windows refuses to delete a file held open.
        platform: Some(Platform::Windows),
    },
    Mutant {
        name: "mutant_git_worktree_clear_unlisted",
        issue: 124,
        breaks: "removal deletes a directory that is not a worktree git lists under the root",
        package: "bosun-git",
        check: Check::Test("worktree::tests::only_a_worktree_git_lists_under_the_root_is_deleted"),
        platform: None,
    },
    // #125: landing.
    Mutant {
        name: "mutant_git_rebase_keep_conflict",
        issue: 125,
        breaks: "a conflicting rebase is left in the worktree",
        package: "bosun-git",
        check: Check::Test("land::tests::a_conflicting_rebase_is_aborted_and_reported"),
        platform: None,
    },
    Mutant {
        name: "mutant_git_ff_dirty_checkout",
        issue: 125,
        breaks: "a fast-forward moves a checkout with local changes",
        package: "bosun-git",
        check: Check::Test("land::tests::a_dirty_checkout_of_the_target_defers_the_fast_forward"),
        platform: None,
    },
    Mutant {
        name: "mutant_git_ff_ignores_eol",
        issue: 125,
        breaks: "a fast-forward writes the user's checkout without its line-ending policy",
        package: "bosund",
        check: Check::Test(
            "executor::tests::landing::a_checkout_with_autocrlf_lands_clean_with_its_line_endings",
        ),
        platform: None,
    },
    // #126: landing modes.
    Mutant {
        name: "mutant_git_squash_skip_tree_check",
        issue: 126,
        breaks: "a squash lands a tree other than the verified one",
        package: "bosun-git",
        check: Check::Test("merge::tests::a_squash_that_differs_from_the_verified_tree_is_refused"),
        platform: None,
    },
    Mutant {
        name: "mutant_git_merge_drop_trailers",
        issue: 126,
        breaks: "a squash or merge commit lands without the Item, Run and Session trailers",
        package: "bosun-git",
        check: Check::Test(
            "merge::tests::each_method_ends_with_the_verified_tree_and_the_trailers",
        ),
        platform: None,
    },
    Mutant {
        name: "mutant_git_merge_signed",
        issue: 126,
        breaks: "a squash or merge commit is signed, so landing waits on a signing program",
        package: "bosun-git",
        check: Check::Test(
            "merge::tests::a_repository_that_signs_its_commits_still_lands_unsigned",
        ),
        platform: None,
    },
    // #137: the reasons of a refusal before the verdict are stored with the attempt.
    Mutant {
        name: "mutant_executor_drop_precondition_reasons",
        issue: 137,
        breaks: "a failed precondition's reasons are not stored: the report and prompt lack them",
        package: "bosund",
        check: Check::Test(
            "executor::tests::a_precondition_refusal_reaches_the_report_and_the_next_prompt",
        ),
        platform: None,
    },
    // #134: the trailer hook acts only in the item's checkout; tests' git drops the session.
    Mutant {
        name: "mutant_git_trailers_everywhere",
        issue: 134,
        breaks: "a commit in another repository made from a session's environment gets the trailers",
        package: "bosun-git",
        check: Check::Test("tests::a_commit_in_another_repository_carries_no_trailer"),
        platform: None,
    },
    Mutant {
        name: "mutant_git_test_env_inherited",
        issue: 134,
        breaks: "a test's git inherits the session's BOSUN_* and GIT_CONFIG_* variables",
        package: "bosun-git",
        check: Check::Test("tests::the_test_git_drops_the_session_variables"),
        platform: None,
    },
    // #147: the branch guard of worktree removal; the fast-forward's descendant check and
    // in-checkout move.
    Mutant {
        name: "mutant_git_worktree_remove_any_branch",
        issue: 147,
        breaks: "removing a worktree deletes a branch outside bosun/",
        package: "bosun-git",
        check: Check::Test("worktree::tests::a_branch_outside_the_prefix_is_never_deleted"),
        platform: None,
    },
    Mutant {
        name: "mutant_git_ff_skip_descendant_check",
        issue: 147,
        breaks: "a fast-forward moves a branch to a commit that does not descend from it",
        package: "bosun-git",
        check: Check::Test(
            "land::tests::a_commit_that_does_not_descend_from_the_branch_is_refused",
        ),
        platform: None,
    },
    Mutant {
        name: "mutant_git_ff_checked_out_by_ref",
        issue: 147,
        breaks: "a fast-forward moves a checked-out branch's ref and leaves its files behind",
        package: "bosun-git",
        check: Check::Test("land::tests::a_clean_checkout_of_the_target_moves_with_its_files"),
        platform: None,
    },
    // #149: landing modes and shared build directories in the configuration.
    Mutant {
        name: "mutant_config_landing_without_worktree",
        issue: 149,
        breaks: "`landing.mode = \"merge\"` is accepted without a worktree",
        package: "bosun-config",
        check: Check::Test("tests::merge_and_pr_need_a_worktree"),
        platform: None,
    },
    Mutant {
        name: "mutant_config_shared_build_any_resource",
        issue: 149,
        breaks: "a shared build directory names a missing or non-exclusive resource",
        package: "bosun-config",
        check: Check::Test(
            "tests::a_shared_build_names_an_exclusive_resource_and_a_directory_outside_the_repository",
        ),
        platform: None,
    },
    // #181: a shared build directory that holds the checkout or the worktrees; `landing.target`
    // without a worktree; git's branch name rules for the target.
    Mutant {
        name: "mutant_config_shared_build_holder_allowed",
        issue: 181,
        breaks: "a shared build directory that holds the repository or the worktree root passes",
        package: "bosun-config",
        check: Check::Test(
            "tests::a_shared_build_holding_the_repository_or_the_worktree_root_is_refused",
        ),
        platform: None,
    },
    Mutant {
        name: "mutant_config_target_kept_without_worktree",
        issue: 181,
        breaks: "landing.target is kept without a worktree, which nothing reads, and no notice says so",
        package: "bosun-config",
        check: Check::Test("tests::a_target_without_a_worktree_is_ignored_with_a_notice"),
        platform: None,
    },
    Mutant {
        name: "mutant_config_target_loose_branch_rules",
        issue: 181,
        breaks: "a landing.target git refuses as a branch name is accepted until landing",
        package: "bosun-config",
        check: Check::Test("tests::a_landing_target_follows_gits_branch_name_rules"),
        platform: None,
    },
    // #150: the worktree, process, lease-wait and landing tables.
    Mutant {
        name: "mutant_store_process_offsets_overwrite",
        issue: 150,
        breaks: "a process's output offsets go back when a smaller offset is written",
        package: "bosun-store",
        check: Check::Test("work::tests::a_worktree_its_process_and_its_landing_read_back"),
        platform: None,
    },
    Mutant {
        name: "mutant_store_lease_wait_duplicates",
        issue: 150,
        breaks: "a replayed lease request is a second wait row, or fails its batch",
        package: "bosun-store",
        check: Check::Test("work::tests::a_lease_wait_is_filed_granted_and_released_once"),
        platform: None,
    },
    // #151: landing by rebase-then-reverify in the item machine.
    Mutant {
        name: "mutant_core_rebased_lands_unverified",
        issue: 151,
        breaks: "a rebased commit lands on the verdict that passed before the rebase",
        package: "bosun-core",
        check: Check::Test("item::tests::a_rebased_commit_is_verified_again_before_it_lands"),
        platform: None,
    },
    Mutant {
        name: "mutant_core_landing_failure_uncounted",
        issue: 151,
        breaks: "a rebased commit failing its verdict starts a new attempt uncounted",
        package: "bosun-core",
        check: Check::Test(
            "item::tests::a_rebased_commit_that_fails_its_verdict_is_a_failed_attempt",
        ),
        platform: None,
    },
    Mutant {
        name: "mutant_core_conflict_sessions_unbounded",
        issue: 151,
        breaks: "every landing conflict starts another session",
        package: "bosun-core",
        check: Check::Test("item::tests::a_second_conflict_needs_the_user"),
        platform: None,
    },
    // #152: the daemon's clone and its item worktrees.
    Mutant {
        name: "mutant_loop_worktree_in_user_repo",
        issue: 152,
        breaks: "an item worktree is made from the user's checkout, not the daemon's clone",
        package: "bosund",
        check: Check::Test(
            "executor::workspace::tests::an_item_worktree_hangs_off_the_daemons_clone_not_the_users_checkout",
        ),
        platform: None,
    },
    Mutant {
        name: "mutant_loop_setup_failure_ignored",
        issue: 152,
        breaks: "a failing worktree setup command is treated as success",
        package: "bosund",
        check: Check::Test(
            "executor::workspace::tests::a_failing_setup_is_reported_and_leaves_no_worktree",
        ),
        platform: None,
    },
    Mutant {
        name: "mutant_loop_quarantine_unrecorded",
        issue: 152,
        breaks: "a quarantined worktree is recorded as removed",
        package: "bosund",
        check: Check::Test(
            "executor::workspace::tests::a_quarantined_worktree_is_recorded_and_the_next_one_gets_a_new_path",
        ),
        // Only Windows refuses to delete a file held open.
        platform: Some(Platform::Windows),
    },
    // #153: the loop runs an item in its worktree; the guard on the user's checkout.
    Mutant {
        name: "mutant_loop_session_in_user_checkout",
        issue: 153,
        breaks: "in worktree mode the session runs in the user's checkout",
        package: "bosund",
        check: Check::Test(
            "executor::tests::worktrees::an_item_runs_in_its_worktree_and_the_users_checkout_is_untouched",
        ),
        platform: None,
    },
    Mutant {
        name: "mutant_loop_skip_checkout_guard",
        issue: 153,
        breaks: "the user's checkout is not compared before the verdict",
        package: "bosund",
        check: Check::Test(
            "executor::tests::worktrees::a_session_that_switches_the_users_branch_needs_the_user",
        ),
        platform: None,
    },
    Mutant {
        name: "mutant_loop_strict_checkout_guard",
        issue: 153,
        breaks: "any change of the user's checkout (a fast-forward, a status line) is a refusal",
        package: "bosund",
        check: Check::Test(
            "executor::tests::worktrees::a_user_commit_and_a_dirty_file_during_the_run_do_not_stop_the_item",
        ),
        platform: None,
    },
    Mutant {
        name: "mutant_loop_setup_failure_is_claim_failure",
        issue: 153,
        breaks: "a failing worktree setup is a failed claim, retried, instead of Needs you",
        package: "bosund",
        check: Check::Test(
            "executor::tests::worktrees::a_failing_setup_needs_the_user_and_starts_no_session",
        ),
        platform: None,
    },
    // #154: the verdict runs in a throwaway checkout of the attempt's head commit.
    Mutant {
        name: "mutant_loop_verdict_in_item_worktree",
        issue: 154,
        breaks: "in worktree mode the verdict runs in the item's worktree, among uncommitted files",
        package: "bosund",
        check: Check::Test("executor::tests::worktrees::the_verdict_sees_only_what_was_committed"),
        platform: None,
    },
    Mutant {
        name: "mutant_loop_verdict_checkout_without_setup",
        issue: 154,
        breaks: "the setup command does not run in a verdict checkout",
        package: "bosund",
        check: Check::Test(
            "executor::tests::worktrees::the_setup_command_runs_in_the_verdict_checkout",
        ),
        platform: None,
    },
    Mutant {
        name: "mutant_loop_keep_verdict_checkout",
        issue: 154,
        breaks: "a verdict checkout is left behind after its run",
        package: "bosund",
        check: Check::Test(
            "executor::tests::worktrees::the_verdict_checkout_is_removed_after_each_run",
        ),
        platform: None,
    }, // #155: the lease manager in the loop; the shared build directory and the mtime rule.
    Mutant {
        name: "mutant_loop_skip_leases",
        issue: 155,
        breaks: "a session starts without waiting for its lease",
        package: "bosund",
        // In worktree mode the claim takes the item's leases and keeps them for its first
        // session, so only a session after a park has to wait for them itself.
        check: Check::Test(
            "executor::tests::leases::a_parked_item_holds_no_lease_and_takes_it_again_to_resume",
        ),
        platform: None,
    },
    Mutant {
        name: "mutant_loop_keep_stale_leases",
        issue: 155,
        breaks: "a journalled lease whose daemon and shim are gone is kept at start",
        package: "bosund",
        check: Check::Test("leases::tests::a_lease_whose_owners_are_gone_is_released_at_start"),
        platform: None,
    },
    Mutant {
        name: "mutant_loop_skip_mtime_touch",
        issue: 155,
        breaks: "a worktree's tracked files are not made newer than the shared build",
        package: "bosund",
        check: Check::Test(
            "executor::workspace::tests::a_worktree_entered_after_another_build_has_its_tracked_files_touched",
        ),
        platform: None,
    },
    Mutant {
        name: "mutant_loop_no_build_env",
        issue: 155,
        breaks: "the shared build directory is not injected into verdict runs",
        package: "bosund",
        check: Check::Test(
            "executor::tests::leases::a_verdict_under_the_shared_build_gets_its_directory_and_fresh_file_times",
        ),
        platform: None,
    },
    // #162: the process sweep of named strays after an attempt (E12).
    Mutant {
        name: "mutant_loop_sweep_ignores_start_time",
        issue: 162,
        breaks: "the process sweep kills a listed process that started before the attempt",
        package: "bosund",
        check: Check::Test(
            "executor::tests::sweep::a_named_stray_started_during_the_attempt_is_killed_and_an_older_one_is_not",
        ),
        platform: None,
    },
    Mutant {
        name: "mutant_loop_sweep_unlogged",
        issue: 162,
        breaks: "the process sweep's kills are neither reported nor noted on the item",
        package: "bosund",
        check: Check::Test(
            "executor::tests::sweep::a_named_stray_started_during_the_attempt_is_killed_and_an_older_one_is_not",
        ),
        platform: None,
    },
    Mutant {
        name: "mutant_proc_named_matches_every_process",
        issue: 162,
        breaks: "processes_named lists every process of the user, whatever its name",
        package: "bosun-proc",
        check: Check::Test("processes_named_finds_a_copy_of_the_helper_by_its_name_only"),
        platform: None,
    },
    Mutant {
        name: "mutant_loop_sweep_kills_other_sessions",
        issue: 162,
        breaks: "the process sweep kills a listed process of another live session (#2)",
        package: "bosund",
        check: Check::Test(
            "executor::tests::sweep::a_stray_of_another_live_session_is_spared_by_the_sweep",
        ),
        platform: None,
    },
    Mutant {
        name: "mutant_api_lease_acquire_always_grants",
        issue: 156,
        breaks: "lease.acquire answers granted without asking the lease manager",
        package: "bosund",
        check: Check::Test(
            "executor::tests::leases::a_command_holding_a_resource_blocks_an_item_until_it_exits",
        ),
        platform: None,
    },
    Mutant {
        name: "mutant_api_lease_without_operator",
        issue: 156,
        breaks: "the lease methods skip the operator token: an agent's own connection takes leases",
        package: "bosund",
        check: Check::Test(
            "executor::tests::leases::a_lease_call_without_the_operator_token_is_refused",
        ),
        platform: None,
    },
    Mutant {
        name: "mutant_cli_with_exit_zero",
        issue: 156,
        breaks: "bosun with exits 0 whatever its command's exit code",
        package: "bosun-cli",
        check: Check::Test("with::tests::the_commands_exit_code_is_the_clis"),
        platform: None,
    },
    Mutant {
        name: "mutant_cli_with_runs_on_refusal",
        issue: 180,
        breaks: "bosun with runs its command although the daemon refused the lease",
        package: "bosun-cli",
        check: Check::Test("with_runs_no_command_when_the_daemon_refuses"),
        platform: None,
    },
    Mutant {
        name: "mutant_cli_with_no_pathext",
        issue: 180,
        breaks: "bosun with spawns a bare name as given: a .cmd or .bat is not found on Windows",
        package: "bosun-cli",
        check: Check::Test("with::tests::a_cmd_file_runs_and_its_exit_code_is_the_clis"),
        platform: Some(Platform::Windows),
    },
    Mutant {
        name: "mutant_cli_with_no_signal_forward",
        issue: 180,
        breaks: "bosun with dies of SIGTERM and its command runs on without the lease",
        package: "bosun-cli",
        check: Check::Test("with_passes_a_termination_on_to_its_command"),
        platform: Some(Platform::Linux),
    },
    Mutant {
        name: "mutant_loop_accept_pre_rebase_verdict",
        issue: 157,
        breaks: "a rebased head lands on the strength of the verdict before the rebase",
        package: "bosund",
        check: Check::Test(
            "executor::tests::landing::a_rebase_that_breaks_the_verdict_does_not_land",
        ),
        platform: None,
    },
    Mutant {
        name: "mutant_loop_land_while_deferred",
        issue: 157,
        breaks: "a deferred fast-forward of the target is reported as landed",
        package: "bosund",
        check: Check::Test(
            "executor::tests::landing::a_dirty_checkout_of_the_target_defers_the_landing_and_holds_the_queue",
        ),
        platform: None,
    },
    Mutant {
        name: "mutant_loop_land_ref_only",
        issue: 157,
        breaks: "landing moves the user's branch by its ref alone, not its checkout",
        package: "bosund",
        check: Check::Test(
            "executor::tests::landing::a_target_that_moved_is_rebased_onto_and_verified_again",
        ),
        platform: None,
    },
    Mutant {
        name: "mutant_loop_shim_in_place",
        issue: 158,
        breaks: "a worktree-mode session runs the shim from where it was found, not a copy",
        package: "bosund",
        check: Check::Test(SHIM_SCENE),
        platform: None,
    },
    Mutant {
        name: "mutant_loop_drop_shim_offsets",
        issue: 158,
        breaks: "the offsets of a shim's output are never written to its process row",
        package: "bosund",
        check: Check::Test(SHIM_SCENE),
        platform: None,
    },
    Mutant {
        name: "mutant_loop_sessions_without_shim",
        issue: 158,
        breaks: "a worktree-mode session is a plain child and dies with the loop",
        package: "bosund",
        check: Check::Test("executor::tests::shim::a_session_outlives_the_loop_that_started_it"),
        platform: None,
    },
    Mutant {
        name: "mutant_loop_shim_cap_unbounded",
        issue: 158,
        breaks: "the shim's cap is a day, whatever the item's wall-clock budget",
        package: "bosund",
        check: Check::Test(
            "executor::session::tests::the_shim_cap_is_the_remaining_wall_budget_or_a_day",
        ),
        platform: None,
    },
    Mutant {
        name: "mutant_loop_reap_instead_of_attach",
        issue: REATTACH_CARD,
        breaks: "a restarted daemon reaps a worktree-mode session and resumes it uncounted",
        package: "bosund",
        check: Check::Test(REATTACH_SCENE),
        platform: None,
    },
    Mutant {
        name: "mutant_loop_reattach_from_zero",
        issue: REATTACH_CARD,
        breaks: "a re-attached session's output is read again from its start",
        package: "bosund",
        check: Check::Test(
            "executor::tests::recovery::a_reattached_session_is_read_from_its_stored_offsets",
        ),
        platform: None,
    },
    Mutant {
        name: "mutant_loop_reattach_without_credential",
        issue: REATTACH_CARD,
        breaks: "a re-attached session's credential is not admitted again",
        package: "bosund",
        check: Check::Test(REATTACH_SCENE),
        platform: None,
    },
    Mutant {
        name: "mutant_core_reattach_counts",
        issue: REATTACH_CARD,
        breaks: "a re-attach counts as an uncounted resume",
        package: "bosun-core",
        check: Check::Test("item::tests::a_reattached_item_keeps_running_and_counts_nothing"),
        platform: None,
    },
    Mutant {
        name: "mutant_runner_prompt_without_conflict",
        issue: CONFLICT_CARD,
        breaks: "the session after a rebase conflict is not told about it",
        package: "bosun-runner",
        check: Check::Test("prompt::tests::a_conflict_is_the_first_thing_the_prompt_says"),
        platform: None,
    },
    Mutant {
        name: "mutant_runner_conflict_files_raw",
        issue: CONFLICT_CARD,
        breaks: "conflicting file names go into the prompt as they are",
        package: "bosun-runner",
        check: Check::Test("prompt::tests::conflicting_file_names_are_listed_as_data"),
        platform: None,
    },
    Mutant {
        name: "mutant_loop_conflict_note_unread",
        issue: CONFLICT_CARD,
        breaks: "the session after a rebase conflict never gets the conflict note",
        package: "bosund",
        check: Check::Test(
            "executor::tests::landing::a_conflict_gets_one_session_with_the_conflict_as_its_prompt_and_then_lands",
        ),
        platform: None,
    },
    Mutant {
        name: "mutant_loop_ambient_commit_identity",
        issue: IDENTITY_CARD,
        breaks: "the landing rebase and commit-tree take the daemon process's global git identity",
        package: "bosund",
        check: Check::Test(
            "executor::tests::landing::the_daemons_commits_carry_the_repository_identity",
        ),
        platform: None,
    },
    Mutant {
        name: "mutant_loop_aside_commit_fixed_identity",
        issue: IDENTITY_CARD,
        breaks: "a worktree's set-aside work is committed as bosun <bosun@localhost>",
        package: "bosund",
        check: Check::Test(
            "executor::tests::worktrees::the_set_aside_work_carries_the_repository_identity",
        ),
        platform: None,
    },
    Mutant {
        name: "mutant_loop_land_over_stale_rebase",
        issue: LANDING_FIX_CARD,
        breaks: "a rebase left in progress in the worktree is not aborted, and its partial pick lands",
        package: "bosund",
        check: Check::Test(
            "executor::tests::landing::a_rebase_left_in_progress_is_aborted_before_landing",
        ),
        platform: None,
    },
    Mutant {
        name: "mutant_store_landing_row_coalesces",
        issue: LANDING_FIX_CARD,
        breaks: "a landing row keeps another attempt's or another head's landed commit",
        package: "bosun-store",
        check: Check::Test(
            "work::tests::a_new_attempt_or_a_new_verified_head_clears_the_landing_row",
        ),
        platform: None,
    },
    Mutant {
        name: "mutant_loop_landed_only_when_equal",
        issue: LANDING_FIX_CARD,
        breaks: "a target the user committed on past the landed commit is rebased onto again",
        package: "bosund",
        check: Check::Test(
            "executor::landing::tests::a_target_past_the_landed_commit_counts_as_landed",
        ),
        platform: None,
    },
    Mutant {
        name: "mutant_loop_keep_landing_ref",
        issue: LANDING_LEAK_CARD,
        breaks: "a landing ref outlives a landing cancelled while deferred",
        package: "bosund",
        check: Check::Test(
            "executor::tests::landing::a_cancelled_deferred_landing_leaves_no_landing_ref",
        ),
        platform: None,
    },
    Mutant {
        name: "mutant_loop_landing_fetch_writes_fetch_head",
        issue: LANDING_LEAK_CARD,
        breaks: "landing's fetch writes the user's FETCH_HEAD and may start maintenance there",
        package: "bosund",
        check: Check::Test("executor::tests::landing::landing_leaves_the_users_fetch_head_alone"),
        platform: None,
    },
    Mutant {
        name: "mutant_git_ff_race_is_failure",
        issue: LANDING_LEAK_CARD,
        breaks: "a branch that moved before its update-ref refuses the landing instead of restarting it",
        package: "bosun-git",
        check: Check::Test(
            "land::tests::a_branch_that_moved_before_its_update_is_reported_as_moved",
        ),
        platform: None,
    },
    Mutant {
        name: "mutant_loop_rebased_unbounded",
        issue: LANDING_LEAK_CARD,
        breaks: "a verified head that cannot be recorded is rebased and verified again without end",
        package: "bosund",
        check: Check::Test(
            "executor::landing::tests::the_same_head_rebased_three_times_is_refused",
        ),
        platform: None,
    },
    Mutant {
        name: "mutant_runner_conflict_files_keep_format_chars",
        issue: LANDING_LEAK_CARD,
        breaks: "conflicting file names keep line separators and invisible format characters",
        package: "bosun-runner",
        check: Check::Test(
            "prompt::tests::conflicting_file_names_lose_invisible_and_line_breaking_characters",
        ),
        platform: None,
    },
    Mutant {
        name: "mutant_loop_stop_kills_shim_sessions",
        issue: DETACH_CARD,
        breaks: "a graceful daemon stop kills worktree-mode sessions instead of leaving them for the next daemon",
        package: "bosund",
        check: Check::Test(
            "executor::tests::shim::a_graceful_stop_leaves_the_session_running_and_the_next_loop_reattaches",
        ),
        platform: None,
    },
    Mutant {
        name: "mutant_loop_process_row_after_spawn",
        issue: DETACH_CARD,
        breaks: "a session's `process` row is written only after its shim started",
        package: "bosund",
        check: Check::Test(
            "executor::tests::shim::the_process_row_is_written_before_the_shim_starts",
        ),
        platform: None,
    },
    Mutant {
        name: "mutant_loop_attach_range_from_base",
        issue: DETACH_CARD,
        breaks: "a re-attached session's commit range starts at the attempt's base, not where the session started",
        package: "bosund",
        check: Check::Test(
            "executor::tests::recovery::a_reattached_session_counts_only_its_own_commits",
        ),
        platform: None,
    },
    // #172: the landing arrows of the item machine after review (restart, kill switch, flaky
    // re-run rule, conflict keys).
    Mutant {
        name: "mutant_core_restart_lands_unverified",
        issue: 172,
        breaks: "a restart during the re-verdict of a rebased commit emits `Land` and strands the item",
        package: "bosun-core",
        check: Check::Test("item::tests::a_daemon_restarted_during_a_re_verdict_runs_it_again"),
        platform: None,
    },
    Mutant {
        name: "mutant_core_kill_switch_skips_landing",
        issue: 172,
        breaks: "the kill switch neither kills nor parks the re-verdict of a rebased commit",
        package: "bosun-core",
        check: Check::Test("item::tests::the_kill_switch_reaches_a_re_verdict_in_landing"),
        platform: None,
    },
    Mutant {
        name: "mutant_core_rebased_ignores_kill_hold",
        issue: 172,
        breaks: "a rebase under the kill switch starts the re-verdict",
        package: "bosun-core",
        check: Check::Test("item::tests::a_rebase_under_the_kill_switch_waits_to_verify"),
        platform: None,
    },
    Mutant {
        name: "mutant_core_reverify_keeps_first_run",
        issue: 172,
        breaks: "a flaky attempt's rebased commit fails on one failing run, with no re-run",
        package: "bosun-core",
        check: Check::Test(
            "item::tests::a_flaky_attempt_gets_the_re_run_rule_on_its_rebased_commit",
        ),
        platform: None,
    },
    Mutant {
        name: "mutant_core_conflict_unkeyed",
        issue: 172,
        breaks: "a late conflict of an earlier landing pass counts as the second conflict",
        package: "bosun-core",
        check: Check::Test("item::tests::a_conflict_from_an_earlier_landing_pass_is_ignored"),
        platform: None,
    }, // #173: what `setup` made in an item's worktree survives the claim and a reset; `setup` and
    // `teardown` end at a kill; a verdict waits below the disk floor; a setup failure's
    // Needs-you comment says what failed.
    Mutant {
        name: "mutant_loop_base_verdict_in_item_worktree",
        issue: 173,
        breaks: "the base verdict runs in the item's worktree and its restore wipes what `setup` made",
        package: "bosund",
        check: Check::Test(
            "executor::tests::worktrees::the_setup_output_is_there_for_the_first_session",
        ),
        platform: None,
    },
    Mutant {
        name: "mutant_loop_reset_without_setup",
        issue: 173,
        breaks: "a fresh session after a reset starts without what `setup` made",
        package: "bosund",
        check: Check::Test(
            "executor::tests::worktrees::a_fresh_session_after_a_reset_finds_the_setup_output",
        ),
        platform: None,
    },
    Mutant {
        name: "mutant_loop_setup_ignores_kill",
        issue: 173,
        breaks: "a cancel or the kill switch waits for a running `setup` or `teardown` to end",
        package: "bosund",
        check: Check::Test("executor::tests::worktrees::a_cancel_during_setup_ends_it_at_once"),
        platform: None,
    },
    Mutant {
        name: "mutant_loop_floor_is_setup_failure",
        issue: 173,
        breaks: "a verdict checkout below the disk floor sends the item to Needs you",
        package: "bosund",
        check: Check::Test(
            "executor::tests::worktrees::a_verdict_below_the_disk_floor_waits_instead_of_needing_the_user",
        ),
        platform: None,
    },
    Mutant {
        name: "mutant_loop_setup_comment_without_cause",
        issue: 173,
        breaks: "a setup failure's Needs-you comment does not say what failed",
        package: "bosund",
        check: Check::Test(
            "executor::tests::worktrees::a_failed_setup_says_what_failed_in_its_comment",
        ),
        platform: None,
    },
    // The worktree sweep: what no item owns is closed, quarantines are retried, setup children
    // are recorded and reaped.
    Mutant {
        name: "mutant_loop_no_worktree_sweep",
        issue: SWEEP_CARD,
        breaks: "a worktree no item in flight owns is never closed",
        package: "bosund",
        check: Check::Test(
            "executor::tests::worktrees::a_worktree_no_item_owns_is_closed_at_the_next_claim",
        ),
        platform: None,
    },
    Mutant {
        name: "mutant_loop_setup_child_unrecorded",
        issue: SWEEP_CARD,
        breaks: "a `setup` a dead daemon started runs on with nobody to end it",
        package: "bosund",
        check: Check::Test(
            "executor::tests::recovery::a_daemon_killed_during_setup_leaves_no_setup_running",
        ),
        platform: None,
    },
    Mutant {
        name: "mutant_loop_quarantine_not_retried",
        issue: SWEEP_CARD,
        breaks: "a quarantined worktree stays on disk after it is free",
        package: "bosund",
        check: Check::Test(
            "executor::tests::worktrees::a_quarantined_worktree_is_removed_once_it_is_free",
        ),
        // Only Windows refuses to delete a file held open.
        platform: Some(Platform::Windows),
    },
    Mutant {
        name: "mutant_git_retry_quarantine_anywhere",
        issue: SWEEP_CARD,
        breaks: "retrying a quarantine deletes a directory outside the worktree root",
        package: "bosun-git",
        check: Check::Test(
            "worktree::tests::a_quarantine_is_retried_only_under_its_root_and_with_its_marker",
        ),
        platform: None,
    },
    Mutant {
        name: "mutant_loop_guard_compares_remotes",
        issue: 175,
        breaks: "a pruned or force-fetched remote-tracking ref sends the item to Needs you",
        package: "bosund",
        check: Check::Test("executor::checkout::tests::only_heads_and_tags_are_compared"),
        platform: None,
    },
    Mutant {
        name: "mutant_loop_clone_without_longpaths",
        issue: 175,
        breaks: "the daemon's clone lacks core.longpaths on Windows",
        package: "bosund",
        check: Check::Test("executor::workspace::tests::the_clone_has_long_paths_on_windows"),
        // The setting is Windows-only, and so is its test.
        platform: Some(Platform::Windows),
    },
    Mutant {
        name: "mutant_loop_clone_without_eol_pin",
        issue: 124,
        breaks: "the daemon's clone takes the machine's line-ending policy, not git's defaults",
        package: "bosund",
        check: Check::Test("executor::workspace::tests::the_clone_pins_the_line_ending_policy"),
        platform: None,
    },
    Mutant {
        name: "mutant_loop_mode_from_config",
        issue: 175,
        breaks: "recovery takes an item's mode from today's configuration and writes to the user's checkout",
        package: "bosund",
        check: Check::Test(
            "executor::tests::recovery::a_worktree_item_recovered_without_the_worktree_key_leaves_the_users_checkout_alone",
        ),
        platform: None,
    },
    Mutant {
        name: "mutant_loop_lease_after_setup",
        issue: LEASE_WINDOW_CARD,
        breaks: "the claim runs setup and the base verdict under the shared build without the lease",
        package: "bosund",
        check: Check::Test(
            "executor::tests::leases::the_setup_and_the_base_verdict_wait_for_the_shared_build",
        ),
        platform: None,
    },
    Mutant {
        name: "mutant_loop_release_before_teardown",
        issue: LEASE_WINDOW_CARD,
        breaks: "an item's teardown runs after its leases are released",
        package: "bosund",
        check: Check::Test(
            "executor::tests::leases::teardown_runs_while_the_item_still_holds_its_leases",
        ),
        platform: None,
    },
    Mutant {
        name: "mutant_loop_halt_keeps_leases",
        issue: LEASE_WINDOW_CARD,
        breaks: "a loop halted by the store keeps its item's leases while the daemon runs",
        package: "bosund",
        check: Check::Test("executor::tests::leases::a_loop_that_halts_frees_its_items_leases"),
        platform: None,
    },
    Mutant {
        name: "mutant_loop_hold_leases_while_parked",
        issue: LEASE_WAIT_CARD,
        breaks: "a parked item keeps its leases until its limit resets",
        package: "bosund",
        check: Check::Test(
            "executor::tests::leases::a_parked_item_holds_no_lease_and_takes_it_again_to_resume",
        ),
        platform: None,
    },
    Mutant {
        name: "mutant_loop_hold_leases_while_deferred",
        issue: LEASE_WAIT_CARD,
        breaks: "an item keeps its leases while its landing's fast-forward is deferred",
        package: "bosund",
        check: Check::Test("executor::tests::leases::a_deferred_landing_holds_no_lease"),
        platform: None,
    },
    Mutant {
        name: "mutant_loop_lease_refusal_fails_verdict",
        issue: LEASE_WAIT_CARD,
        breaks: "a lease refusal before the verdict is a failed run that counts an attempt",
        package: "bosund",
        check: Check::Test(
            "executor::tests::leases::a_lease_refusal_before_the_verdict_needs_the_user_and_counts_no_attempt",
        ),
        platform: None,
    },
    Mutant {
        name: "mutant_leases_one_journal_per_directory",
        issue: 179,
        breaks: "every daemon of a user reads and overwrites one lease journal",
        package: "bosund",
        check: Check::Test("leases::tests::two_daemons_on_two_endpoints_keep_two_journals"),
        platform: None,
    },
    Mutant {
        name: "mutant_leases_adopt_running_daemon",
        issue: 179,
        breaks: "a new daemon adopts the journalled leases of another daemon that still runs",
        package: "bosund",
        check: Check::Test("leases::tests::an_entry_of_another_running_daemon_is_not_adopted"),
        platform: None,
    },
    Mutant {
        name: "mutant_leases_reask_keeps_old_daemon",
        issue: 179,
        breaks: "a re-attached item's lease is released when its session's shim ends",
        package: "bosund",
        check: Check::Test(
            "leases::tests::a_holder_that_asks_again_after_a_restart_keeps_its_lease_when_its_shim_ends",
        ),
        platform: None,
    },
    Mutant {
        name: "mutant_leases_overwrite_corrupt_journal",
        issue: 179,
        breaks: "a lease journal that does not parse is overwritten instead of moved aside",
        package: "bosund",
        check: Check::Test("leases::tests::a_journal_that_does_not_parse_is_moved_aside"),
        platform: None,
    },
    Mutant {
        name: "mutant_api_lease_holder_across_identities",
        issue: RESOURCES_CARD,
        breaks: "lease.list names another identity's holder instead of saying busy",
        package: "bosund",
        check: Check::Test(
            "view_tests::the_resource_list_shows_another_identitys_holder_only_as_busy",
        ),
        platform: None,
    },
    Mutant {
        name: "mutant_api_wait_unnamed",
        issue: RESOURCES_CARD,
        breaks: "status.overview and item.get do not say what a waiting item waits for",
        package: "bosund",
        check: Check::Test("view_tests::a_waiting_item_names_the_resource_and_its_holder"),
        platform: None,
    },
    Mutant {
        name: "mutant_tui_resources_tab_blank",
        issue: RESOURCES_CARD,
        breaks: "the TUI's Resources tab draws no row",
        package: "bosun-tui",
        check: Check::Test("view::tests::the_resources_tab_lists_holders_waits_and_worktrees"),
        platform: None,
    },
    Mutant {
        name: "mutant_report_drop_lease_waits",
        issue: RESOURCES_CARD,
        breaks: "the report omits its lease waits section",
        package: "bosun-cli",
        check: Check::Cargo(&[
            "test",
            "--package",
            "bosun-cli",
            "--locked",
            "--bin",
            "bosun",
            "--",
            "markdown::tests::the_report_has_lease_waits_and_worktree_disk_use",
            "--exact",
        ]),
        platform: None,
    },
    Mutant {
        name: "mutant_loop_pr_create_without_lookup",
        issue: PR_CARD,
        breaks: "a pull request landing opens a pull request without looking one up by its head",
        package: "bosund",
        check: Check::Test(PR_REDO_SCENE),
        platform: None,
    },
    Mutant {
        name: "mutant_forge_adopts_fork_pull_request",
        issue: PR_CARD,
        breaks: "a fork's pull request of the item's branch name is taken for the item's",
        package: "bosund",
        check: Check::Test("executor::forge::tests::a_pull_request_from_a_fork_is_never_adopted"),
        platform: None,
    },
    Mutant {
        name: "mutant_loop_pr_push_without_lease",
        issue: PR_CARD,
        breaks: "a head rebased since its branch was pushed cannot be pushed: Needs you",
        package: "bosund",
        check: Check::Test(
            "executor::tests::landing::a_rebased_head_is_pushed_over_the_branch_pushed_before",
        ),
        platform: None,
    },
    Mutant {
        name: "mutant_forge_tool_without_timeout",
        issue: PR_CARD,
        breaks: "a `gh` call or forge git that hangs stalls the repository's loop",
        package: "bosund",
        check: Check::Test("executor::forge::tests::a_tool_that_hangs_is_killed_and_unreachable"),
        platform: None,
    },
    Mutant {
        name: "mutant_loop_pr_based_on_local_target",
        issue: PR_CARD,
        breaks: "a pull request branch is based on the user's local target, unpushed commits and all",
        package: "bosund",
        check: Check::Test(
            "executor::tests::landing::a_pull_request_branch_never_holds_the_users_unpushed_commits",
        ),
        platform: None,
    },
    Mutant {
        name: "mutant_loop_pr_message_range_local_target",
        issue: PR_CARD,
        breaks: "in `pr` mode the commits the forge's target has over the user's local one are judged as the item's",
        package: "bosund",
        check: Check::Test(
            "executor::tests::landing::a_forge_target_ahead_of_the_users_is_not_the_items_own_work",
        ),
        platform: None,
    },
    Mutant {
        name: "mutant_loop_pr_forge_target_unfollowed",
        issue: PR_CARD,
        breaks: "the forge's target fetched at landing is taken for a moved ref by the next preconditions",
        package: "bosund",
        check: Check::Test(
            "executor::tests::landing::a_conflict_with_the_forge_target_lands_in_one_attempt",
        ),
        platform: None,
    },
    Mutant {
        name: "mutant_loop_pr_refusal_is_retried",
        issue: PR_CARD,
        breaks: "a refused push or pull request is retried as if the forge had not answered",
        package: "bosund",
        check: Check::Test(
            "executor::tests::landing::a_refused_pull_request_needs_the_user_and_an_unreachable_forge_keeps_landing",
        ),
        platform: None,
    },
    Mutant {
        name: "mutant_loop_pr_body_overwritten",
        issue: PR_CARD,
        breaks: "a redone pull request landing replaces the body people edited",
        package: "bosund",
        check: Check::Test(PR_REDO_SCENE),
        platform: None,
    },
    Mutant {
        name: "mutant_loop_pr_open_releases_blockers",
        issue: PR_POLL_CARD,
        breaks: "a Done blocker in `pr` mode releases its dependants while its pull request is open",
        package: "bosund",
        check: Check::Test(
            "executor::tests::landing::a_dependant_waits_for_the_merge_of_its_blockers_pull_request",
        ),
        platform: None,
    },
    Mutant {
        name: "mutant_loop_done_ignored_while_pending",
        issue: PR_POLL_CARD,
        breaks: "a user's drag to Done is lost while any column write of the item is pending",
        package: "bosund",
        check: Check::Test("executor::commands::tests::a_board_behind_the_outbox_is_not_a_change"),
        platform: None,
    },
    Mutant {
        name: "mutant_loop_pr_closed_stored_after_event",
        issue: PR_POLL_CARD,
        breaks: "a closed pull request's row stays open when its event is not written",
        package: "bosund",
        check: Check::Test(
            "executor::tests::landing::a_closed_pull_request_is_stored_before_its_event",
        ),
        platform: None,
    },
    Mutant {
        name: "mutant_loop_pr_closed_ignored",
        issue: PR_POLL_CARD,
        breaks: "a pull request closed unmerged is taken for an open one: the item stays Done",
        package: "bosund",
        check: Check::Test(
            "executor::tests::landing::a_pull_request_closed_unmerged_needs_the_user_with_one_comment",
        ),
        platform: None,
    },
    Mutant {
        name: "mutant_core_pr_closed_from_anywhere",
        issue: PR_POLL_CARD,
        breaks: "a closed pull request moves an item to Needs you from any state",
        package: "bosun-core",
        check: Check::Test(
            "item::tests::a_closed_pull_request_moves_a_done_item_to_needs_you_once",
        ),
        platform: None,
    },
    Mutant {
        name: "mutant_test_slowdown_ignored",
        issue: 165,
        breaks: "a scene wait ignores `BOSUN_TEST_SLOWDOWN`: a slowed run times out",
        package: "bosund",
        check: Check::Test("executor::tests::timing::the_slowdown_factor_stretches_a_scene_wait"),
        platform: None,
    },
    Mutant {
        name: "mutant_test_daily_cap_auto_pause",
        issue: 165,
        breaks: "the daily-cap scene keeps the automatic pause: started near midnight, it times out",
        package: "bosund",
        check: Check::Test(
            "executor::tests::timing::the_daily_cap_scene_started_at_23_59_local_runs_after_midnight",
        ),
        platform: None,
    },
    Mutant {
        name: "mutant_proc_children_inherit_console",
        issue: 131,
        breaks: "the daemon's console children share its console: once its host is gone, they fail at load",
        package: "bosund",
        check: Check::Test("console_tests::children_start_after_the_hosting_console_has_ended"),
        // Only a Windows console can be gone from under its process.
        platform: Some(Platform::Windows),
    },
    Mutant {
        name: "mutant_log_panics_without_stderr",
        issue: 131,
        breaks: "a log line panics when standard error is a console that is gone",
        package: "bosund",
        check: Check::Test("console_tests::children_start_after_the_hosting_console_has_ended"),
        platform: Some(Platform::Windows),
    },
    Mutant {
        name: "mutant_store_repo_by_path",
        issue: 132,
        breaks: "a loop matches stored items by path: a moved repository's items are lost and a                  card of one is claimed as new, against its stored transitions",
        package: "bosund",
        check: Check::Test(
            "executor::tests::moved::a_moved_repository_keeps_its_items_and_their_history",
        ),
        platform: None,
    },
    Mutant {
        name: "mutant_store_repo_new_row_on_move",
        issue: 132,
        breaks: "a configured repository at a new path gets a second row: its history splits",
        package: "bosun-store",
        check: Check::Test(
            "writer::tests::a_moved_repository_keeps_its_row_and_both_paths_attribute_to_it",
        ),
        platform: None,
    },
    Mutant {
        name: "mutant_queue_lint_references_unwarned",
        issue: 139,
        breaks: "a card that leans on an issue the session cannot read passes lint silently",
        package: "bosun-queue",
        check: Check::Test(
            "lint::tests::a_bare_issue_reference_in_the_acceptance_or_the_verdict_warns",
        ),
        platform: None,
    },
    Mutant {
        name: "mutant_queue_lint_provenance_warns",
        issue: 139,
        breaks: "a provenance line (`Parent:`, `Part of`) warns like a reference the card leans on",
        package: "bosun-queue",
        check: Check::Test(
            "lint::tests::provenance_lines_and_references_outside_the_acceptance_do_not_warn",
        ),
        platform: None,
    },
    Mutant {
        name: "mutant_runner_prompt_without_provenance_rule",
        issue: 139,
        breaks: "the session is not told that it cannot read GitHub and that a `#N` is provenance only",
        package: "bosun-runner",
        check: Check::Test(
            "prompt::tests::the_system_prompt_says_issue_references_are_provenance_only",
        ),
        platform: None,
    },
];

/// #164, the card of #8 that polls the pull request of an item Done in `pr` mode (merged
/// releases dependants, closed unmerged is Needs you).
const PR_POLL_CARD: u32 = 164;

/// #163, the card of #8 that lands an item as a pull request (`landing.mode = "pr"`), and the
/// mutants of its review.
const PR_CARD: u32 = 163;

/// The redo scene of the pull request card, which hosts two mutants.
const PR_REDO_SCENE: &str =
    "executor::tests::landing::a_landing_redone_after_the_pull_request_opened_opens_no_second_one";

/// #161, the card of #8 that shows the leases (`lease.list`, the TUI's Resources tab, what a
/// waiting item waits for) and puts lease waits and worktree disk use in the report.
const RESOURCES_CARD: u32 = 161;

/// The card of #8 that lets an item go of its leases while it is Parked or its landing is
/// deferred, and takes a lease refusal for a configuration change (E24; its own number was not
/// known when it was written; its parent's stands in).
const LEASE_WAIT_CARD: u32 = 8;

/// The card of #8 that leases an item's resources from before its worktree's setup to after
/// its teardown, and frees them when the loop halts (its own number was not known when it was
/// written; its parent's stands in).
const LEASE_WINDOW_CARD: u32 = 8;

/// The card of #8 that sweeps the worktrees no item owns, retries quarantines and records the
/// children of `setup` and `teardown` (its own number was not known when it was written; its
/// parent's stands in).
const SWEEP_CARD: u32 = 8;

/// The card of #8 that leaves worktree-mode sessions running at a graceful stop, writes their
/// `process` row before the spawn and records where a session's commit range starts (its own
/// number was not known when it was written; its parent's stands in).
const DETACH_CARD: u32 = 8;

/// The card of #8 that makes the daemon's commits carry the repository identity (E26; its own
/// number was not known when it was written; its parent's stands in).
const IDENTITY_CARD: u32 = 8;

/// The card of #8 that tells the session after a rebase conflict about it (its own number was
/// not known when it was written; its parent's stands in).
const CONFLICT_CARD: u32 = 8;

/// The card of #8 that re-attaches worktree-mode sessions after a daemon restart (its own
/// number was not known when it was written; its parent's stands in).
const REATTACH_CARD: u32 = 8;

/// The card of #8 that keeps landing from landing a partial rebase, another attempt's landed
/// commit, or a landing twice (its own number was not known when it was written; its parent's
/// stands in).
const LANDING_FIX_CARD: u32 = 8;

/// The card of #8 that keeps landing from leaking its ref, writing the user's `FETCH_HEAD`,
/// refusing a racing target, looping on an unrecordable verified head, or quoting invisible
/// characters (its own number was not known when it was written; its parent's stands in).
const LANDING_LEAK_CARD: u32 = 8;

/// The re-attach scene of the card, which hosts two mutants.
const REATTACH_SCENE: &str = "executor::tests::recovery::a_daemon_killed_mid_session_reattaches_and_finishes_with_one_attempt_and_one_run";

/// The shim scene of #158, which hosts two mutants.
const SHIM_SCENE: &str =
    "executor::tests::shim::a_session_in_worktree_mode_runs_under_a_copied_shim_and_is_recorded";

/// The guard and trailer scenario of #99, which hosts several mutants.
const SCENARIO_99: &str =
    "executor::tests::the_git_guards_refuse_before_the_verdict_and_the_trailers_reach_the_report";

/// The agent.* scenario of #63, which hosts two mutants.
const SCENARIO_63: &str = "executor::tests::agent_calls_are_bound_capped_recorded_and_a_blocking_question_ends_the_session";

/// The scenario suite of the loop (#62), which hosts several mutants.
const SCENARIO_62: &str =
    "executor::tests::seven_items_end_done_or_needs_you_and_board_edits_take_effect";

/// What one side (without or with the cfg) of a mutant run produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    /// The package's tests did not build; the check never ran.
    Unbuilt,
    /// Built, and the check failed.
    Red,
    /// Built, and the check passed.
    Green,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    BaselineBuildFailed,
    BaselineRed,
    BuildFailed,
    StaysGreen,
    RedAsExpected,
}

impl Outcome {
    fn describe(self) -> &'static str {
        match self {
            Self::BaselineBuildFailed => "the tests do not build without the mutant",
            Self::BaselineRed => "the check is red without the mutant",
            Self::BuildFailed => "build failed with the mutant (a broken build is not red)",
            Self::StaysGreen => "the check stays green with the mutant",
            Self::RedAsExpected => "red as expected",
        }
    }
}

fn judge(baseline: Step, mutated: Step) -> Outcome {
    match (baseline, mutated) {
        (Step::Unbuilt, _) => Outcome::BaselineBuildFailed,
        (Step::Red, _) => Outcome::BaselineRed,
        (Step::Green, Step::Green) => Outcome::StaysGreen,
        (Step::Green, Step::Unbuilt) if !cfg!(mutant_xtask_no_build_step) => Outcome::BuildFailed,
        (Step::Green, Step::Unbuilt | Step::Red) => Outcome::RedAsExpected,
    }
}

/// Runs `cargo` with the mutant's cfg when `mutated`. Returns whether it succeeded.
fn cargo(mutant: &Mutant, mutated: bool, args: &[&str]) -> Result<bool, String> {
    let mut flags = std::env::var("RUSTFLAGS").unwrap_or_default();
    let label = if mutated {
        flags.push_str(" --cfg ");
        flags.push_str(mutant.name);
        mutant.name
    } else {
        "baseline"
    };
    // One target directory per cfg, so that the changed RUSTFLAGS do not invalidate the main
    // build cache.
    let target_dir = workspace_root().join("target").join("mutants").join(label);
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned());
    let status = Command::new(cargo)
        .args(args)
        .current_dir(workspace_root())
        .env("RUSTFLAGS", flags.trim())
        .env("CARGO_TARGET_DIR", target_dir)
        .status()
        .map_err(|e| format!("cannot run cargo: {e}"))?;
    Ok(status.success())
}

fn builds(mutant: &Mutant, mutated: bool) -> Result<bool, String> {
    cargo(
        mutant,
        mutated,
        &["test", "--package", mutant.package, "--no-run", "--locked"],
    )
}

fn check_passes(mutant: &Mutant, mutated: bool) -> Result<bool, String> {
    match mutant.check {
        Check::Test(test) => cargo(
            mutant,
            mutated,
            &[
                "test",
                "--package",
                mutant.package,
                "--locked",
                "--",
                test,
                "--exact",
            ],
        ),
        Check::Cargo(args) => cargo(mutant, mutated, args),
    }
}

fn step(mutant: &Mutant, mutated: bool) -> Result<Step, String> {
    Ok(if !builds(mutant, mutated)? {
        Step::Unbuilt
    } else if check_passes(mutant, mutated)? {
        Step::Green
    } else {
        Step::Red
    })
}

pub(crate) fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf)
}

/// Command-line selection: `[--shard K/N] [--platform-only] [NAME...]`.
#[derive(Debug, Default, PartialEq, Eq)]
struct Selection {
    /// One-based shard index and shard count.
    shard: Option<(usize, usize)>,
    /// Run only the mutants bound to the host platform.
    platform_only: bool,
    names: Vec<String>,
}

fn parse(args: &[String]) -> Result<Selection, String> {
    let mut selection = Selection::default();
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--shard" => {
                let value = args.next().ok_or("--shard needs K/N")?;
                let (k, n) = value
                    .split_once('/')
                    .and_then(|(k, n)| Some((k.parse().ok()?, n.parse().ok()?)))
                    .filter(|&(k, n)| n > 0 && (1..=n).contains(&k))
                    .ok_or_else(|| format!("bad shard {value:?}, expected K/N with 1 <= K <= N"))?;
                selection.shard = Some((k, n));
            }
            "--platform-only" => selection.platform_only = true,
            flag if flag.starts_with("--") => return Err(format!("unknown flag {flag}")),
            name => selection.names.push(name.to_owned()),
        }
    }
    Ok(selection)
}

/// The mutants this host runs. Platform-bound mutants run only on their platform; with
/// `--platform-only`, nothing else runs. Shards split the selection round-robin.
fn select<'a>(all: &'a [Mutant], selection: &Selection, host: Option<Platform>) -> Vec<&'a Mutant> {
    all.iter()
        .filter(|m| selection.names.is_empty() || selection.names.iter().any(|n| n == m.name))
        .filter(|m| match m.platform {
            Some(platform) => Some(platform) == host,
            None => !selection.platform_only,
        })
        .enumerate()
        .filter(|(i, _)| selection.shard.is_none_or(|(k, n)| i % n == k - 1))
        .map(|(_, m)| m)
        .collect()
}

pub(crate) fn run(args: &[String]) -> Result<(), String> {
    let selection = parse(args)?;
    if let Some(unknown) = selection
        .names
        .iter()
        .find(|n| !MUTANTS.iter().any(|m| m.name == n.as_str()))
    {
        return Err(format!("no mutant named {unknown}"));
    }
    let selected = select(MUTANTS, &selection, Platform::host());
    if selected.is_empty() {
        println!("no mutant selected for this host and shard");
        return Ok(());
    }

    let mut failures = Vec::new();
    for mutant in selected {
        println!("== {} (#{}): {}", mutant.name, mutant.issue, mutant.breaks);
        let outcome = judge(step(mutant, false)?, step(mutant, true)?);
        if outcome == Outcome::RedAsExpected {
            println!("== {}: {}", mutant.name, outcome.describe());
        } else {
            failures.push(format!("{}: {}", mutant.name, outcome.describe()));
        }
    }

    if failures.is_empty() {
        return Ok(());
    }
    for line in &failures {
        eprintln!("  {line}");
    }
    Err(format!("{} mutant(s) failed", failures.len()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn a_build_failure_is_not_red() {
        assert_eq!(judge(Step::Green, Step::Unbuilt), Outcome::BuildFailed);
    }

    #[test]
    fn judge_covers_every_outcome() {
        assert_eq!(judge(Step::Green, Step::Red), Outcome::RedAsExpected);
        assert_eq!(judge(Step::Green, Step::Green), Outcome::StaysGreen);
        assert_eq!(judge(Step::Red, Step::Red), Outcome::BaselineRed);
        assert_eq!(
            judge(Step::Unbuilt, Step::Red),
            Outcome::BaselineBuildFailed
        );
    }

    fn mutant(name: &'static str, platform: Option<Platform>) -> Mutant {
        Mutant {
            name,
            issue: 0,
            breaks: "",
            package: "p",
            check: Check::Cargo(&[]),
            platform,
        }
    }

    fn names(selected: &[&Mutant]) -> Vec<&'static str> {
        selected.iter().map(|m| m.name).collect()
    }

    #[test]
    fn selection_by_platform_and_shard() {
        let all = [
            mutant("mutant_a", None),
            mutant("mutant_b", Some(Platform::Windows)),
            mutant("mutant_c", None),
            mutant("mutant_d", None),
        ];
        let linux = Some(Platform::Linux);
        let windows = Some(Platform::Windows);
        let all_on = |s: &Selection, host| names(&select(&all, s, host));

        let any = Selection::default();
        assert_eq!(all_on(&any, linux), ["mutant_a", "mutant_c", "mutant_d"]);
        assert_eq!(
            all_on(&any, windows),
            ["mutant_a", "mutant_b", "mutant_c", "mutant_d"]
        );

        let platform_only = parse(&["--platform-only".to_owned()]).unwrap();
        assert_eq!(all_on(&platform_only, linux), Vec::<&str>::new());
        assert_eq!(all_on(&platform_only, windows), ["mutant_b"]);

        let first = parse(&["--shard".to_owned(), "1/2".to_owned()]).unwrap();
        let second = parse(&["--shard".to_owned(), "2/2".to_owned()]).unwrap();
        assert_eq!(all_on(&first, linux), ["mutant_a", "mutant_d"]);
        assert_eq!(all_on(&second, linux), ["mutant_c"]);
    }

    #[test]
    fn bad_arguments_are_refused() {
        for bad in [
            &["--shard", "0/2"][..],
            &["--shard", "3/2"],
            &["--shard"],
            &["--x"],
        ] {
            let args: Vec<String> = bad.iter().map(|s| (*s).to_owned()).collect();
            assert!(parse(&args).is_err(), "{bad:?}");
        }
    }

    /// Names of the form `mutant_<a-z0-9_>` that appear in `text` right after `cfg(`,
    /// `cfg(not(`, `cfg!(` or `--cfg `.
    fn cfg_names(text: &str) -> BTreeSet<String> {
        let mut found = BTreeSet::new();
        for prefix in ["cfg(not(", "cfg(", "cfg!(", "--cfg "] {
            for (at, _) in text.match_indices(prefix) {
                let rest = &text[at + prefix.len()..];
                let len = rest
                    .find(|c: char| !(c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_'))
                    .unwrap_or(rest.len());
                let name = &rest[..len];
                if name.len() > "mutant_".len() && name.starts_with("mutant_") {
                    found.insert(name.to_owned());
                }
            }
        }
        found
    }

    fn sources(dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if path.file_name().is_some_and(|n| n == "target") {
                    continue;
                }
                sources(&path, out);
            } else if path.extension().is_some_and(|e| e == "rs" || e == "toml") {
                out.push(path);
            }
        }
    }

    #[test]
    fn registry_lint_list_and_source_agree() {
        let root = workspace_root();
        let registered: BTreeSet<String> = MUTANTS.iter().map(|m| m.name.to_owned()).collect();

        let manifest = std::fs::read_to_string(root.join("Cargo.toml")).unwrap();
        let start = manifest
            .find("unexpected_cfgs")
            .expect("the root manifest declares unexpected_cfgs");
        let end = start
            + manifest[start..]
                .find(']')
                .expect("the check-cfg list closes");
        let declared = cfg_names(&manifest[start..end]);

        let mut files = Vec::new();
        sources(&root.join("crates"), &mut files);
        sources(&root.join("xtask"), &mut files);
        let mut used = BTreeSet::new();
        for file in files {
            used.extend(cfg_names(&std::fs::read_to_string(file).unwrap()));
        }

        assert_eq!(registered, declared, "MUTANTS vs the check-cfg list");
        assert_eq!(registered, used, "MUTANTS vs the cfgs used in the source");
    }

    #[test]
    fn cfg_names_ignores_placeholders() {
        // Built at run time so this file does not itself contain the names it tests with,
        // which would trip `registry_lint_list_and_source_agree`.
        let c = "cfg";
        let text = format!("--{c} mutant_<name> {c}(mutant_a) {c}(not(mutant_b_2)) {c}(windows)");
        let expected: BTreeSet<String> = ["mutant_a", "mutant_b_2"].map(str::to_owned).into();
        assert_eq!(cfg_names(&text), expected);
    }
}
