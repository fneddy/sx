# sx Query Cheat Sheet

Queries are one or more space-separated **terms**. Every term is either a
`namespace:value` filter or a bare word. All terms are ANDed together by
default.

---

## Bare terms

A word with no namespace matches files **named** or **containing** that word.

```
readme
"error handling"      # quote to include spaces
```

---

## Combinators

| Syntax | Meaning |
|--------|---------|
| `a b c` | a AND b AND c (default) |
| `or(a,b,c)` | a OR b OR c |
| `not(a)` | NOT a |

```
or(ext:rs,ext:go)
not(path:vendor)
ext:ts not(path:node_modules)
```

---

## File & path filters

### `name:` · `n:`  — filename
Match the filename (last segment only).

```
name:main
n:!test          # does NOT contain "test"
n:"my file"
```

### `path:` · `p:`  — full path
Match anywhere in the full path.

```
path:src/tests
p:!vendor
p:/home/eddy/docs   # absolute path → added as a search root (alongside the current directory)
```

### `ext:` · `e:`  — file extension
Comma-separated allow-list. Case-insensitive.

```
ext:rs
e:rs,py,go
```

### `type:` · `t:`  — entry type

| Value | Short | Meaning |
|-------|-------|---------|
| `file` | `f` | Regular file |
| `dir` | `d` | Directory |
| `symlink` | `l` | Symbolic link |
| `empty` | `e` | File with size = 0 |
| `executable` | `x` | Execute bit set |
| `binary` | `b` | Detected as binary |
| `text` | `t` | Detected as text |

```
t:f
type:dir
t:executable
```

### `hidden:` · `h:`  — dot-files

| Value | Aliases | Behaviour |
|-------|---------|-----------|
| `true` | `include` | Include hidden entries |
| `false` | `exclude` | Exclude hidden entries *(default)* |
| `only` | | Only hidden entries |

```
h:include
hidden:only
```

---

## Size

### `size:` · `s:`
Operators: `>` `<` `=` (default is `=`). Units: `b` `k`/`kb` `m`/`mb` `g`/`gb`. Decimals are allowed.

```
s:>10m
size:<1k
s:1.5m
s:0          # empty files
```

---

## Timestamps

Three namespaces with identical syntax:

| Namespace | Aliases | What it compares |
|-----------|---------|-----------------|
| `modified:` | `mod:` `m:` | Last-modified time |
| `created:` | `cr:` | Creation time |
| `accessed:` | `ac:` | Last-access time |

### Value forms

| Form | Example | Meaning |
|------|---------|---------|
| `Nd` / `Nw` / `Nm` / `Ny` | `7d` `2w` `3m` `1y` | Within the last N days/weeks/months/years |
| `today` | `today` | During today |
| `YYYY-MM-DD` | `2024-06-15` | During that calendar day |
| `>…` / `<…` / `=…` | `>30d` `<2024-01-01` | Explicit comparison — `>` means the file timestamp is **more recent than** the reference point; `<` means **older than** |

Without an explicit `>/<` prefix, a relative value means "more recent than" (same as `>`):

```
m:7d              # modified in the last 7 days
mod:today
modified:2024-06-15
m:<30d            # NOT modified in the last 30 days (older than 30 days)
ac:>1w            # accessed less than 1 week ago (more recent than 1 week ago)
```

---

## Permissions  *(Unix only)*

### `perm:`
Octal exact match or has-bit check.

```
perm:644          # mode == 644
perm:0755
perm:+x           # any execute bit set
perm:+w
perm:+r
```

---

## Owner & group  *(Unix only)*

### `owner:`
Username or UID.

```
owner:eddy
owner:1000
owner:!root       # not owned by root
```

### `group:`
Group name or GID.

```
group:docker
group:!nobody
```

---

## Content filters

These require the file to be read.

### `content:` · `c:`  — whole-file search
The pattern is searched anywhere in the file.

```
c:TODO
content:"error handling"
c:!FIXME          # files NOT containing "FIXME"
```

### `line:` · `l:`  — per-line search
Each `line:` term is matched per line. Multiple `line:` terms in the same
query must all appear **on the same line**.

```
l:TODO
l:foo l:bar       # a single line that contains both
l:!FIXME
```

### `mime:`  — MIME type (magic bytes)
Matched against the MIME type detected from the file header, not the extension.

```
mime:pdf
mime:image/png
mime:!text
```

---

## Handler namespaces

These invoke external handlers rather than searching the filesystem.

### `calc:` · `=:`  — calculator
Evaluates a mathematical expression.

```
calc:2^32
calc:sqrt(2)
calc:sin(pi/2)
=:1920/1080
```

### `weather:`  — current weather
City name or `lat,lon` coordinates.

```
weather:berlin
weather:48.1,11.6
```

### `apps:`  — installed applications
Searches installed desktop apps by name or application ID.

```
apps:firefox
apps:text
apps:            # list all
```

### `localsearch:`  — system file index
Full-text search via the LocalSearch/Tracker3 daemon. Multiple words are ANDed.

```
localsearch:readme
localsearch:report 2024
```

### `sys:`  — system information
Three sub-commands:

```
sys:process:nginx       # running processes matching "nginx"
sys:port:8080           # what is listening on port 8080
sys:disk:/home          # disk usage for /home
```

---

## Practical examples

```
# Rust source files modified in the last week
ext:rs m:7d

# Large files not in target or vendor directories
s:>100m not(path:target) not(path:vendor)

# Python files containing "TODO" and owned by me
ext:py c:TODO owner:eddy

# Find all executables named "server" anywhere in /usr
n:server t:executable p:/usr

# Files changed today that aren't hidden or test files
mod:today h:exclude n:!test

# A line that mentions both "panic" and "unwrap"
l:panic l:unwrap

# Office documents or PDFs larger than 5 MB
or(ext:pdf,ext:docx,ext:xlsx) s:>5m

# Any file whose MIME is an image, excluding thumbnails
mime:image not(path:thumbnail)

# World-writable files owned by root (potential misconfiguration)
owner:root perm:+w
```

---

## Quick-reference card

```
BARE        word  "quoted phrase"

NAME        name:foo    n:!foo      n:"my file"
PATH        path:src    p:!vendor   p:/abs/root
EXT         ext:rs,py   e:go
TYPE        t:f  t:d  t:l  t:e  t:x  t:b  t:t
HIDDEN      h:include  h:exclude  h:only

SIZE        s:>10m  s:<1k  s:1.5m  s:0
MODIFIED    m:7d  m:today  m:2024-06-15  m:<30d
CREATED     cr:7d  cr:today
ACCESSED    ac:1w  ac:today

PERM        perm:644  perm:+x  perm:+w
OWNER       owner:eddy  owner:!root
GROUP       group:docker

CONTENT     c:foo  c:!bar  c:"two words"
LINE        l:foo  l:foo l:bar   (same-line AND)
MIME        mime:pdf  mime:image/png  mime:!text

CALC        calc:sqrt(2)  =:2^32
WEATHER     weather:berlin  weather:48.1,11.6
APPS        apps:firefox
SEARCH      localsearch:readme
SYS         sys:process:name  sys:port:8080  sys:disk:/

COMBINE     or(a,b,c)   not(a)   a b c  (AND)
```
