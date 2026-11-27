# bosun heat metrics: personal

Range: 2001-09-09T01:46:40Z to 2001-09-10T01:46:40Z (end excluded). Scope: every repository.

| repository | provider | kind | metric | value | unit | total | sample | exact or estimate | comparable across runners |
| --- | --- | --- | --- | ---: | --- | ---: | ---: | --- | --- |
| alpha | claude | interactive | cache_read_share | 49.0% | ppm | 2400 | 4900 prompt_token | exact | no, within one provider |
| alpha | claude | interactive | context_at_start | 2400 | tokens | 2400 | 1 session | exact | no, within one provider |
| alpha | claude | interactive | finding_bytes_per_session | 500 | bytes_per_session | 500 | 1 session | exact | yes |
| alpha | claude | interactive | finding_tokens_per_session | 125 | estimated_tokens_per_session | 125 | 1 session | estimate (bytes_per_token = 4) | no, within one provider |
| alpha | claude | interactive | rereads_per_session | 0 | reads_per_session | 0 | 1 session | exact | yes |
| alpha | claude | interactive | tool_error_rate | 0.0% | ppm | 0 | 2 call | exact | yes |
| alpha | claude | run | cache_read_share | 44.6% | ppm | 9000 | 20200 prompt_token | exact | no, within one provider |
| alpha | claude | run | context_at_start | 3000 | tokens | 9000 | 2 session | exact | no, within one provider |
| alpha | claude | run | eqv_per_commit | 8625 | eqv_tokens_per_commit | 17250 | 2 commit | exact | yes |
| alpha | claude | run | finding_bytes_per_session | 2103 | bytes_per_session | 4206 | 2 session | exact | yes |
| alpha | claude | run | finding_tokens_per_session | 526 | estimated_tokens_per_session | 1053 | 2 session | estimate (bytes_per_token = 4) | no, within one provider |
| alpha | claude | run | flaky_verdict_rate | 50.0% | ppm | 1 | 2 closed_item | exact | yes |
| alpha | claude | run | parked_ms_per_item | 450000 | ms_per_closed_item | 900000 | 2 closed_item | exact | yes |
| alpha | claude | run | rereads_per_session | 1 | reads_per_session | 2 | 2 session | exact | yes |
| alpha | claude | run | tool_error_rate | 10.0% | ppm | 1 | 10 call | exact | yes |

