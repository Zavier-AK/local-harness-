# Third-party notices

The skills in this directory are adapted from **David Ondrej's agent skills**:
<https://github.com/davidondrej/skills>, used under the MIT License.

```
MIT License

Copyright (c) David Ondrej

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
```

## What was adapted, and how

| Ours | Source | Change |
|---|---|---|
| `worktree` | `agent-orchestration/git-worktree` | Substantially cut. The original teaches an agent to create, merge and remove its own worktrees; here the harness does all of that, and a worker doing it itself would corrupt the run. What remains is the part a worker still needs: knowing it is in a disposable tree, and that nothing it writes lands without a human approving the diff. |
| `risky-changes` | `ops-and-setup/risky-changes` | Lightly adapted. The original's core claim — passing tests do not tell you a change is *useful* — is kept, with the evidence step rewritten around what a worker in this harness can actually reach. |
| `review` | `agent-orchestration/total-review` | Written fresh from the original's idea: merge several reviewers' findings, drop duplicates, and report how many were discarded as overthinking. The discard count is the part worth keeping — it is what stops a review turning into noise. |

Only three of the ~45 skills in that repository are carried here. The rest are either
bound to tooling this project does not use or are personal working style; vendoring them
wholesale would be cargo-culting rather than reuse.
