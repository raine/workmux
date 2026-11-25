  # Worktree handles in the current repository
  def "nu-complete workmux handles" [] {
    ^workmux _complete-handles | complete | get stdout | lines
  }

  # Local worktree handles plus agents running in other projects
  def "nu-complete workmux agent-targets" [] {
    ^workmux _complete-agent-targets | complete | get stdout | lines
  }

  # Git branches, for choosing what a new worktree checks out
  def "nu-complete workmux git-branches" [] {
    ^workmux _complete-git-branches | complete | get stdout | lines
  }

