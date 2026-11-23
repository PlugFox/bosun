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
];

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
