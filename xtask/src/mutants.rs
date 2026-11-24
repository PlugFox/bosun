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
        issue: 67,
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
];

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
