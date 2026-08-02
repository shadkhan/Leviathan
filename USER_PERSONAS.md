Here's the value analysis, grounded in who actually hits the "my JSON viewer just froze" wall and what would make them keep Leviathan open all day rather than uninstall after one use.

Personas and their use cases

1. Priya — Backend/API Engineer
   Debugging a production API that returns a 300MB response dump. She needs to find one malformed record among 200,000. Today she either loads it into a viewer that freezes, or drops to jq in a terminal (powerful but no visual tree, painful for exploring unknown structure). Leviathan's value: she gets a browsable tree and a JSONPath query, without leaving the browser or crashing the tab. Value: high — this is the sharpest, most frequent pain, and it's her daily work.

2. Rahul — Data Engineer / ETL
   Works with NDJSON exports from data pipelines — Kafka dumps, BigQuery exports, log streams. Files are routinely 500MB–2GB and line-delimited. He needs to validate structure, spot schema drift, and check for duplicate records before a load job. Leviathan's NDJSON auto-detection + dedup + schema validation is directly his pre-flight check. Value: very high — NDJSON at this scale has almost no good client-side tool; this is Leviathan's least-contested niche.

3. Sofia — QA / Test Engineer
   Validates API contract responses against expected schemas. She has a JSON Schema and a pile of captured responses. She needs "does this response conform, and if not, exactly where does it break." Leviathan's byte-accurate validation + schema check is her assertion tool without writing a test harness. Value: medium-high — recurring, but she has some alternatives (test frameworks).

4. Marcus — Platform/SRE Engineer
   Investigating an incident, staring at a giant structured log export or a Kubernetes/Terraform state file (these get huge and deeply nested). He needs to navigate depth-20 nesting fast and extract a subtree. Leviathan's virtualized tree + breadcrumb + subtree export is his forensic tool. Value: high — incident time is expensive; speed here is real money.

5. Aisha — Integration Developer
   Wiring up a third-party API she's never seen, working from a massive sample payload instead of docs. She needs to understand the shape of unfamiliar data — what fields exist, what's optional, how it nests. Leviathan's tree + a "infer structure/shape" view is her documentation. Value: medium-high — very common task, currently done by squinting.

6. Dev (you / open-source maintainer)
   The meta-persona: the hiring team member who installs it to judge you. Their use case is "is this person a real systems engineer." The WASM benchmark table and clean ADRs are what they consume. Value: this is the entire point of the project.

How much value, honestly

The value is concentrated, not broad. Leviathan is not a tool millions use daily — it's a tool that a specific professional reaches for at a specific painful moment (the file that froze everything else) and is intensely grateful in that moment. That's actually the good kind of value for a portfolio piece: narrow, deep, defensible, and the users are your professional peers/hiring pool. A tool that saves an engineer 40 minutes during an incident earns permanent toolbar space. That's the bar.

The realistic ceiling: this is a respected niche utility with steady dev installs and strong word-of-mouth in the right circles — not a mass-market hit, and it shouldn't try to be. Its job is credibility, and it does that job well.

Features that make it dependable (trust) vs. useful (reach)

Split deliberately, because "dependable" matters more than "more features" for your goals.

Dependability features — these earn trust and belong soon:

Never-crash guarantee, visibly. Graceful handling of malformed/truncated files: show what parsed, flag where it broke, never white-screen. This is the whole brand — "survives large files" means survives broken large files too.
Byte/line/col-accurate error location. When validation fails, jump-to-position in the tree. This is the difference between a toy and a tool.
Bounded memory with a visible indicator. Show file size, parse progress, and memory headroom. Users trusting it with a 1GB file want to see it's not about to die.
Fully offline / no network, provably. The manifest requests zero host permissions; state that loudly. For engineers pasting production data, "this never phones home" is a feature.
Deterministic export. Round-trip fidelity (what you export re-parses identically). Quietly critical for trust.

Usefulness features — pick a few, resist the rest:

Diff two JSON files — structural diff over the index. High value (compare API v1 vs v2 response), and it reuses the engine. Strong v1.5 candidate.
Infer/summarize schema — generate a JSON Schema from a sample, or show a "shape" overview (Aisha's use case). High value, distinctive.
Flatten to table / CSV preview — for arrays-of-objects, a spreadsheet-like view. Rahul and Priya both want this.
Search-in-values (not just JSONPath) — plain full-text find across the whole file, streamed. Low effort, high everyday utility.
Saved queries / history — remember JSONPath expressions per session. Small, sticky.
