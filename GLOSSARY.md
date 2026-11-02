# kiewer

A site where people upload KiCad designs and share them, Discord first. The browser renders the design; the server stores it.

## Language

### Things people own

**Design**:
The thing a person owns and shares: a name, a description, branches, and people with roles on it.
_Avoid_: project (collides with KiCad projects, GCP projects and Cargo projects), board, repo

**Description**:
What a design's owner or admin writes about it, in Markdown, shown rendered on the design's page.
_Avoid_: readme, about, notes, summary

**Revision**:
One immutable snapshot of a design's files, with its parent revision, author, time and optional message, identified by a hash.
_Avoid_: commit, version, snapshot, upload

**Branch**:
A named pointer to the latest revision in one line of work. A branch is always a linear chain. Named with ASCII letters, digits, hyphens, underscores and dots, up to 64 characters, not starting with a hyphen or a dot: one URL segment.

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
A design's name in its URL, unique per owner: suggested by the name when the design is made, editable by owner or admin. A slug the design left redirects to it until another design of the same owner takes the slug.
_Avoid_: path, short name, id

**Owner**:
The one person a design belongs to. Alone may delete, transfer and change visibility.

**Admin**:
A person who may push to any branch of a design, manage people, and edit name and description.

**Collaborator**:
A person who may push revisions to a design and manage their own branches.
_Avoid_: contributor, member, editor

**Account**:
What a person holds on kiewer: one handle, the sign-in methods that open it, the designs it owns. One per person.
_Avoid_: user, profile

**Identity**:
One sign-in method of an account: a provider and that provider's own id for the person. An identity belongs to one account for good; an email address never finds an account.
_Avoid_: connection, linked account, social login, login

**Session**:
A signed-in browser: a row behind a cookie, with its sliding expiry, listed and revocable in settings.
_Avoid_: login, token

**Session key**:
The stored hash the sessions page names a session by, as against the session id the cookie carries.

**Public**:
A design every listing may show: the home page's recent designs, its owner's page.
_Avoid_: visible, listed

**Unlisted**:
A design anyone with the link can open and nothing lists.

**Transfer**:
The owner handing a design to another person, who becomes its owner while the previous owner stays on as an admin.
_Avoid_: reassign, hand over

**People page**:
Where an admin or the owner manages who has which role on a design.
_Avoid_: members page, collaborators page, access settings

### Viewing

**View**:
A named stack of layers with colours, as the eight views in the hand-off. The viewer offers them as presets.
_Avoid_: preset, layer set

**Run**:
A chain of connected, near-collinear track segments of one net on one layer. The unit a net label is placed on.

**Net label**:
Text the viewer writes on a board item as it zooms in: a pad's number, or the net of a pad, a via, a run or a zone. Placed once when the board loads; the label gate decides when each shows.
_Avoid_: label (a schematic's net name on a wire), annotation, overlay

**Label class**:
Which of the five things a net label names: a pad number, a pad net, a via net, a track net or a zone net. The unit the toggles show or hide, and the rank when two labels collide: pads first, then vias, zones, tracks.

**Label gate**:
The rule that a net label draws only when its text height on screen reaches the threshold.
_Avoid_: level of detail, LOD

**Pole of inaccessibility**:
The point inside a zone's fill farthest from every edge of it; where the zone's net label sits.
_Avoid_: centroid, centre (both can fall outside a fill with a hole)

**Hit test**:
The answer to what copper is under a point on the board: a pad over a via over a track over a zone, on the layers the view shows, within a few pixels of the pointer.
_Avoid_: picking, selection

**Net highlight**:
The viewer showing one net lit and everything else dimmed: the net under the pointer, or the pinned one.
_Avoid_: selection, focus

**Pinned net**:
The net a click fixed the highlight on; it stays through pan, zoom and hover until Escape or a click on empty space.
_Avoid_: selected net, locked net

**Stroke font**:
Newstroke, KiCad's built-in line font, bundled in kiewer and used for every text item that names no face. Its glyph table is regenerated from the font's CC0 sources by `newstroke_dev`.

**System face**:
A font face a design names that the browser has, or that the person picked for one it lacks; its text draws from that font rather than from the stroke font. The choice is remembered per browser per face.

**Glyph atlas**:
One image per system face holding every character the design draws in it, redrawn at a size fit for the zoom; text in a system face is drawn from it.

**Knockout**:
A text item drawn inverted: a filled box around the text with the glyphs cut out of it, as KiCad's `knockout` flag on a text's layer asks.

**Upright**:
A footprint text's rule that its drawn angle is folded into (-90, 90], so it reads from the left or from below whatever way the footprint turns. KiCad's default; `(unlocked yes)` in the file turns it off.

### Schematic

**Sheet**:
One page of a schematic: one `.kicad_sch` file, with its paper size, its placed symbols, wires and labels.
_Avoid_: page, schematic (the whole set of sheets)

**Root sheet**:
The sheet a schematic opens on, named after the project; every other sheet hangs below it through a sheet symbol.
_Avoid_: top sheet, main sheet

**Sheet symbol**:
A box on a sheet standing for another sheet, with sheet pins on its edges where that sheet's hierarchical labels come out.
_Avoid_: hierarchical sheet, subsheet

**Library symbol**:
The drawing of a part (its body, pins and texts) as a sheet's own embedded library holds it; every placed symbol refers to one by name.
_Avoid_: lib symbol, part, component

**Symbol**:
A library symbol placed on a sheet: a position, a rotation, a mirror, the unit shown, and its fields.
_Avoid_: component, instance

**Power symbol**:
A symbol whose value is a net name, so placing it joins its pin to that net everywhere: `GND`, `+3V3`.
_Avoid_: power port, power flag (a different symbol)

**Field**:
A named text a symbol or a sheet symbol carries and may show: the reference, the value, the sheet name.
_Avoid_: property, attribute

**Wire**:
A line on a sheet joining pins and labels into one net.
_Avoid_: trace, track (board terms)

**Bus**:
A thick line on a sheet carrying several nets at once; a wire joins it through a bus entry.

**Junction**:
The dot where three or more wires meet, or two cross and connect.
_Avoid_: node

**No-connect**:
The cross on a pin that says it is left unconnected on purpose.
_Avoid_: NC mark

**Label**:
A net name written on a wire. A local label names the net on its own sheet; a global label names it on every sheet; a hierarchical label names it to the parent sheet, where it appears as a sheet pin.
_Avoid_: net label, net name (the net itself)

**Flag**:
The outline around a global label, a hierarchical label or a sheet pin, shaped by its direction: pointed at the wire for an input, away for an output, both ways for a bidirectional or tri-state, a plain box for a passive.
_Avoid_: shape, arrow
