# Migrating to the xan-aligned moonblade language

The expression language used by `derive` (and now by the GUI's **moonblade**
search) was a loose approximation of [`xan`'s
moonblade](https://github.com/medialab/xan/blob/master/docs/moonblade/cheatsheet.md).
It is now aligned with xan: the same operators, the same string/number split and
the same callable forms. This guide lists every behaviour change and shows how to
port existing expressions. Most files need at most a couple of edits.

## TL;DR

| If you wrote… | …change it to | Why |
| --- | --- | --- |
| `a == "foo"` | `a eq "foo"` | `==` is now a **numerical** comparison |
| `a != "foo"` | `a ne "foo"` | `!=` is now numerical |
| `"a" < "b"` (strings) | `"a" lt "b"` | `<` is now numerical |
| `a + b` to join text | `a ++ b` | `+` is numerical only |
| `coalesce(a, b)` | `a \|\| b` (or keep `coalesce`) | `coalesce` is not a xan function |

Everything else keeps working unchanged.

## 1. String and numeric operators are now separate

Before, both families of operators ran the same comparison: try to parse the
operands as numbers and fall back to string comparison. xan keeps them apart, and
so does this crate now.

### Comparisons

| xan operator | Meaning | Example |
| --- | --- | --- |
| `==`, `!=`, `<`, `<=`, `>`, `>=` | **numerical** (both sides are cast to a number) | `count >= 10`, `"2" < "10"` → `false` |
| `eq`, `ne`, `lt`, `le`, `gt`, `ge` | **string**/sequence comparison | `name eq "alice"`, `"2" lt "10"` → `true` |
| `=` | alias for `==` (numerical) | `col1 - col2 = col3` |

Port a string comparison by swapping the symbol for its word form:

```diff
-  derive = 'if(is_default == "Y", "Y", "") as default_key'
+  derive = 'if(is_default eq "Y", "Y", "") as default_key'
```

```diff
-  derive = 'if(status != "open", "closed", "open") as state'
+  derive = 'if(status ne "open", "closed", "open") as state'
```

Numerical operators accept values that look like numbers, so `"2" == 2` is
`true`. When **either side cannot be cast to a number the result is the empty
value** (falsy) instead of falling back to a string test — xan raises an error,
this engine deliberately stays infallible. That means `"foo" == "foo"` is now
empty rather than `true`; use `eq` for it.

### `+` no longer concatenates

`+` is numerical addition, and only that. Concatenation is `++`:

```diff
-  derive = 'first_name + " " + last_name as full_name'
+  derive = 'first_name ++ " " ++ last_name as full_name'
```

The old string fallback meant `"a" + "b"` produced `"ab"`. It now produces the
empty value; rewrite it as `"a" ++ "b"`.

## 2. New callable forms of the logical and comparison operators

xan exposes `and`, `or`, `not`, `eq`, `ne`, `lt`, `le`, `gt` and `ge` as
functions too. They are now accepted here:

```text
and(count > 0, status eq "open")        # same as `count > 0 and status eq "open"`
or(a, b)                                # first truthy value (null OR semantics)
not(is_deleted)                         # boolean negation
eq(name, "alice")                       # same as `name eq "alice"`
ge(score, 0.5)
```

`and`/`or` fold like the `&&`/`||` operators: `or` returns the first truthy
value (or the last falsy one), `and` returns the first falsy value (or the last
one). `typeof(value)` returns `none`, `boolean`, `integer`, `float`, `string`,
`list` or `regex`.

## 3. Comments and new string escapes

`#` starts a comment that runs to the end of the line, so multi-line expressions
can be documented (the lexer already ignored newlines inside balanced brackets):

```text
# pick the X.Y version token
row.match(/\d+\.\d+/) or "" as version
```

`\xNN` and `\u{...}` escapes are now understood inside string literals, matching
xan:

```text
"\x41"        # "A"
"\u{1F60A}"   # "😊"
```

## 4. Things that still differ from xan

The language remains an intentionally small subset. It does **not** (yet)
support maps `{...}`, lambdas / higher-order functions (`map`, `filter`),
binary strings (`b"..."`), unsure identifiers (`text?`) or xan's standard
function library (`fmt`, `read`, date, url, …). `coalesce(a, b, …)` is kept as a
convenience even though xan spells it `a || b`.

`count` and `length` remain aliases of `len`. xan reserves `count(string,
substring)` for counting occurrences, so prefer `len(x)` in new expressions to
avoid a future clash.

## 5. The new moonblade search mode (GUI)

`fview`'s top bar now has **Regex** / **Moonblade** mode tabs. In moonblade
mode the text is an expression evaluated once per row; a truthy result keeps the
row. Unlike the live regex filter, the expression is only evaluated when you
press **Search** (or Enter), so a half-typed expression is never run. This is
how you write the searches the regex box cannot express:

```text
count > 10
lower(name) eq "alice"
status in ["failed", "skipped"]
contains(email, /@example\.com$/)
startswith(col("Full Name"), "A")
```

As in a `derive` expression, columns are referenced by name, `col("name")` /
`col(index)` reach names with spaces or duplicate headers, `#` starts a comment,
and an unknown column or syntax error is shown in the status line instead of
silently matching everything. The **Index** mode and **visible only**
mask do not apply in moonblade mode.

## 6. Migration checklist

1. Search your `.vl` files for `==`, `!=`, `<`, `<=`, `>`, `>=` and decide,
   per occurrence, whether it compares strings (use `eq`/`ne`/`lt`/… ) or
   numbers (leave it).
2. Search for `+` where at least one operand is text and replace it with `++`.
3. Replace `coalesce(a, b)` with `a || b` if you want strict xan compatibility
   (`coalesce` keeps working).
4. Rename any `length(...)` / `count(...)` used as a length to `len(...)`.
5. Run `fvalidate` and check the derived-value reports for rows that stopped
   matching; a `derive` that silently became empty is the usual symptom of a
   missed `==` → `eq` change.
6. In `fview`, select the **Moonblade** tab to try the same
   expressions interactively before committing them to a rule file.
