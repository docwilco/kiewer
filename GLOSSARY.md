# kiewer

A site where people upload KiCad designs and share them, Discord first. The browser renders the design; the server stores it.

## Language

### Things people own

**Design**:
The thing a person owns and shares: a name, a description, branches, and people with roles on it.
_Avoid_: project (collides with KiCad projects, GCP projects and Cargo projects), board, repo

**Revision**:
One immutable snapshot of a design's files, with its parent revision, author, time and optional message, identified by a hash.
_Avoid_: commit, version, snapshot, upload

**Branch**:
A named pointer to the latest revision in one line of work. A branch is always a linear chain.

**Default branch**:
The branch a bare design URL shows. Named `main` unless renamed.

**Head**:
The revision a branch currently points at.

**Ordinal**:
How many revisions deep a revision sits along the branch in context ("rev 7"). A property of the branch, never part of a URL.

**Fork**:
A new design, under another owner or the same one, whose branches start at another design's revisions. Records where it came from.
_Avoid_: copy, clone

**Pending revision**:
An upload whose files are stored but whose branch head moved during the upload, waiting for the uploader to choose re-append or branch. Expires after an hour.

**Forked from**:
The source design and revision a fork started at.

### Files and storage

**Blob**:
The bytes of one file, identified by the SHA-256 of the uncompressed bytes.

**Tree**:
A revision's map from file path to blob hash.

**Project folder**:
The folder an upload is rooted at: the one holding the `.kicad_pro`, else the `.kicad_pcb`, else the shallowest `.kicad_sch`. Paths in a tree are relative to it.

**Blocked blob**:
A blob hash the operator has refused site-wide: not accepted, not served, not rendered.
_Avoid_: banned file, takedown entry

**Collector**:
The nightly job that removes revisions no branch reaches, then trees and blobs no revision names.
_Avoid_: garbage collector, GC, cleanup

### People

**Handle**:
A person's unique name on kiewer, chosen once at first sign-in. The first segment of a design URL.
_Avoid_: username, login

**Slug**:
A design's name in its URL, unique per owner, editable; old slugs redirect.

**Owner**:
The one person a design belongs to. Alone may delete, transfer and change visibility.

**Admin**:
A person who may push to any branch of a design, manage people, and edit name and description.

**Collaborator**:
A person who may push revisions to a design and manage their own branches.
_Avoid_: contributor, member, editor

**Unlisted**:
A design anyone with the link can open and nothing lists.

### Viewing

**View**:
A named stack of layers with colours, as the eight views in the hand-off. The viewer offers them as presets.
_Avoid_: preset, layer set

**Run**:
A chain of connected, near-collinear track segments of one net on one layer. The unit a net label is placed on.

**Label gate**:
The rule that a net label draws only when its text height on screen reaches the threshold.
_Avoid_: level of detail, LOD

**Stroke font**:
Newstroke, KiCad's built-in line font, bundled in kiewer and used for every text item that names no face. Its glyph table is regenerated from the font's CC0 sources by `newstroke_dev`.

**Knockout**:
A text item drawn inverted: a filled box around the text with the glyphs cut out of it, as KiCad's `knockout` flag on a text's layer asks.

**Upright**:
A footprint text's rule that its drawn angle is folded into (-90, 90], so it reads from the left or from below whatever way the footprint turns. KiCad's default; `(unlocked yes)` in the file turns it off.
