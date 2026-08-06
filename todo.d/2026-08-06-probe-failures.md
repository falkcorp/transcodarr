- [ ] **TODO-PROBE** Investigate the 2,271 files that failed to probe — 1,105
      anime, 316 movies, 850 tv, totalling roughly 11.9 TiB. The handoff
      predicted 8, so the rate is not "normal" and was never checked. The
      failures skew enormously large: movies averages ~29 GiB per failed file.
      Worse than the count is where they land — `(not evaluated)`, not
      `Quarantined`, so they are invisible to the dispatcher rather than refused
      once with a reason. Rejecting with a reason instead of silent invisibility
      is a founding requirement of this project and the specific Tdarr behaviour
      it exists to replace. Pull the failing paths and recorded reasons out of
      `~/tc/tc.db` on the server, group by reason and container, probe a handful
      by hand with `ffprobe`, and decide whether this is a prober defect or
      genuinely unreadable media. Either way make the outcome explicit. Rule out
      first: probe timeout at `--probe-concurrency 32` under load (large files
      take longest), ZFS latency on 40-80 GB files, and the `-read_intervals`
      class of bug that PR #41 already fixed once in `last_packet_pts_us`.
