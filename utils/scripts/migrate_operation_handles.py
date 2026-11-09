#!/usr/bin/env python3
"""Inventory old Rust operation bindings and rewrite proven sites into a copy.

Usage: migrate_operation_handles.py ROOT --manifest manifest.json --report report.json
       migrate_operation_handles.py ROOT --manifest manifest.json --report report.json \
           --output-dir COPY

The tool never writes ROOT. COPY must be outside ROOT. Unresolved sites remain
unchanged and are reported with source locations. This is a token codemod, not a
Rust type checker: receiver methods and impl bodies require a separate migration.
"""

import argparse
import collections
import dataclasses
import json
import re
from pathlib import Path


@dataclasses.dataclass(frozen=True)
class Token:
    text: str
    start: int
    end: int


def tokens(source):
    """Keep Rust token offsets, omitting comments and preserving string tokens."""
    result = []
    i = 0
    while i < len(source):
        if source[i].isspace():
            i += 1
            continue
        if source.startswith("//", i):
            end = source.find("\n", i)
            i = len(source) if end < 0 else end
            continue
        if source.startswith("/*", i):
            depth = 1
            i += 2
            while i < len(source) and depth:
                if source.startswith("/*", i):
                    depth += 1
                    i += 2
                elif source.startswith("*/", i):
                    depth -= 1
                    i += 2
                else:
                    i += 1
            if depth:
                raise ValueError("unterminated block comment")
            continue
        start = i
        raw = re.match(r'(?:br|r)(#*)"', source[i:])
        if raw:
            stop = '"' + raw[1]
            end = source.find(stop, i + raw.end())
            if end < 0:
                raise ValueError("unterminated raw string")
            i = end + len(stop)
        elif source[i] == '"' or (source[i] == "'" and re.match(r"'(?:\\.|[^'\\])'", source[i:])):
            quote = source[i]
            i += 1
            while i < len(source):
                if source[i] == "\\":
                    i += 2
                elif source[i] == quote:
                    i += 1
                    break
                else:
                    i += 1
        else:
            match = re.match(r"(?:r#)?[A-Za-z_][A-Za-z_0-9]*|[0-9]+|::|->|=>|\.\.=|\.\.", source[i:])
            i += len(match[0]) if match else 1
        result.append(Token(source[start:i], start, i))
    return result


def pairs(ts):
    stack, result = [], {}
    for i, token in enumerate(ts):
        if token.text in ("(", "[", "{"):
            stack.append(i)
        elif token.text in (")", "]", "}"):
            if not stack or ts[stack[-1]].text != {")": "(", "]": "[", "}": "{"}[token.text]:
                raise ValueError(f"unbalanced delimiter at byte {token.start}")
            opening = stack.pop()
            result[opening] = i
    if stack:
        raise ValueError(f"unclosed delimiter at byte {ts[stack[-1]].start}")
    return result


def split(ts, separator=","):
    result, start, i = [], 0, 0
    ps = pairs(ts)
    while i < len(ts):
        if i in ps:
            i = ps[i] + 1
        elif ts[i].text == separator:
            result.append(ts[start:i])
            start = i + 1
            i += 1
        else:
            i += 1
    if start < len(ts):
        result.append(ts[start:])
    return result


def spelling(ts):
    return "".join(t.text for t in ts)


def literal(ts):
    if len(ts) == 1 and ts[0].text.startswith('"'):
        return json.loads(ts[0].text)
    return None


def fields(ts):
    return {part[0].text: part[2:] for part in split(ts) if len(part) > 2 and part[1].text == ":"}


def symbol(name):
    name = re.sub(r"[^a-zA-Z0-9_]", "_", name) or "op"
    return name if re.match(r"[a-zA-Z_]", name) else "op_" + name


def location(path, source, offset):
    return {"file": path, "line": source.count("\n", 0, offset) + 1,
            "column": offset - source.rfind("\n", 0, offset)}


def invocations(ts, name):
    ps = pairs(ts)
    for i in range(len(ts) - 2):
        if ts[i].text == name and ts[i + 1].text == "!" and i + 2 in ps:
            yield i, i + 2, ps[i + 2]


def operation(alias, body, path, source, offset, origin):
    fs = fields(body)
    name, dialect = literal(fs.get("name", [])), literal(fs.get("dialect", []))
    if not name or not dialect or "$" in alias:
        return None
    accessors = []
    for group in ("attributes", "regions"):
        value = fs.get(group, [])
        if len(value) > 2 and value[1].text == "{":
            for item in split(value[2:-1]):
                if item and re.fullmatch(r"[a-zA-Z_][a-zA-Z_0-9]*", item[0].text):
                    accessors.append({"method": item[0].text, "group": group,
                                      "function": symbol(name) + "_" + item[0].text})
    if fs.get("results"):
        accessors.append({"method": "result", "group": "results", "function": symbol(name) + "_result"})
    return {"alias": alias, "operation": f"{dialect}.{name}", "dialect": dialect,
            "name": name, "constructor": "r#" + symbol(name), "descriptor": symbol(name).upper(),
            "accessors": accessors, "accessor_inventory": "rust_declaration",
            "origin": origin, **location(path, source, offset)}


def macro_arms(body):
    ps, result, i = pairs(body), [], 0
    while i < len(body):
        if i in ps and ps[i] + 2 < len(body) and body[ps[i] + 1].text == "=>":
            end = ps[i]
            output = end + 2
            if output in ps:
                result.append((body[i + 1:end], body[output + 1:ps[output]]))
                i = ps[output] + 1
                continue
        i += 1
    return result


def expand_macro(arms, arguments):
    """Match only top-level comma parameters; unsupported matchers stay unresolved."""
    args = split(arguments)
    for matcher, output in arms:
        pattern = split(matcher)
        # Optional trailing rest cannot change the binding name or basic ports.
        if len(args) < len(pattern) - 1:
            continue
        substitutions, valid = {}, True
        for index, parameter in enumerate(pattern):
            if index >= len(args):
                if any(t.text == "?" for t in parameter):
                    continue
                valid = False
                break
            plain = [t.text for t in parameter]
            if "$" not in plain:
                if spelling(parameter) != spelling(args[index]):
                    valid = False
                    break
                continue
            captures = re.findall(r"\$([a-zA-Z_][a-zA-Z_0-9]*):", spelling(parameter))
            if len(captures) != 1:
                # Interface/rest repetitions do not determine operation ports.
                continue
            capture = captures[0]
            dollar = plain.index("$")
            prefix = parameter[:dollar]
            if spelling(args[index][:len(prefix)]) != spelling(prefix):
                valid = False
                break
            value = args[index][len(prefix):]
            if plain[0] == "[" and value and value[-1].text == "]":
                value = value[:-1]
            substitutions[capture] = value
        if not valid:
            continue
        # Expand the common $( $port ),+ list, then plain metavariables.
        text = " ".join(t.text for t in output)
        marker = r"\$\s*\(\s*impl\s+\$\s*(\w+)\s+for\s+\$\s*(\w+)\s*\{\s*\}\s*\)\s*\*"
        def marker_impls(match):
            interfaces = substitutions.get(match[1])
            operation_type = substitutions.get(match[2])
            if interfaces is None or operation_type is None:
                return match[0]
            return " ".join("impl " + " ".join(token.text for token in interface)
                            + " for " + " ".join(token.text for token in operation_type) + " {}"
                            for interface in split(interfaces))
        text = re.sub(marker, marker_impls, text)
        for name, value in substitutions.items():
            text = re.sub(r"\$\s*\(\s*\$\s*" + re.escape(name) + r"\s*\)\s*,\s*[+*]",
                          lambda _: " ".join(t.text for t in value), text)
            text = re.sub(r"\$\s*" + re.escape(name) + r"\b", lambda _: " ".join(t.text for t in value), text)
        return tokens(text)
    return None


def inventory(files):
    operations, unresolved, protected = [], [], {}
    global_macros = collections.defaultdict(list)
    for source in files.values():
        stream = tokens(source)
        ps = pairs(stream)
        for i in range(len(stream) - 3):
            if stream[i].text == "macro_rules" and stream[i + 1].text == "!" and i + 3 in ps:
                global_macros[stream[i + 2].text].append(macro_arms(stream[i + 4:ps[i + 3]]))
    unique_macros = {name: definitions[0] for name, definitions in global_macros.items() if len(definitions) == 1}
    for path, source in files.items():
        ts, regions, macros = tokens(source), [], dict(unique_macros)
        ps = pairs(ts)
        for i in range(len(ts) - 3):
            if ts[i].text == "macro_rules" and ts[i + 1].text == "!" and i + 3 in ps:
                end = ps[i + 3]
                macros[ts[i + 2].text] = macro_arms(ts[i + 4:end])
                regions.append((ts[i].start, ts[end].end))
        wrappers = {name for name, arms in macros.items()
                    if any(list(invocations(output, "operation")) for _, output in arms)}
        for _ in range(len(macros)):
            previous = len(wrappers)
            wrappers.update(name for name, arms in macros.items()
                            if any(any(list(invocations(output, other)) for other in wrappers)
                                   for _, output in arms))
            if len(wrappers) == previous:
                break
        def record(stream, offset=None, origin="operation!"):
            for _, opening, end in invocations(stream, "operation"):
                body = stream[opening + 1:end]
                if len(body) > 2 and body[1].text == "{":
                    alias = body[0].text
                    pos = offset if offset is not None else body[0].start
                    op = operation(alias, body[2:-1], path, source, pos, origin)
                    if op:
                        op["impls"] = []
                        stream_pairs = pairs(stream)
                        for at, token in enumerate(stream):
                            if token.text != "impl":
                                continue
                            opening = at + 1
                            while opening < len(stream) and stream[opening].text != "{":
                                opening += 1
                            header = stream[at + 1:opening]
                            if header and header[-1].text == alias and opening in stream_pairs:
                                close = stream_pairs[opening]
                                op["impls"].append({"header": " ".join(t.text for t in header),
                                                    "methods": [stream[j + 1].text for j in range(opening + 1, close - 1) if stream[j].text == "fn"]})
                        operations.append(op)
        for _, opening, end in invocations(ts, "operation"):
            if not any(a <= ts[opening].start < b for a, b in regions):
                record(ts[opening - 2:end + 1])
                regions.append((ts[opening - 2].start, ts[end].end))
        for name, arms in macros.items():
            if name not in wrappers:
                continue
            for start, opening, end in invocations(ts, name):
                if any(a <= ts[start].start < b for a, b in regions):
                    continue
                expanded = expand_macro(arms, ts[opening + 1:end])
                if expanded:
                    # Forwarding arms such as arithmetic_conversion!(float, ...).
                    for _ in range(3):
                        nested = list(invocations(expanded, name))
                        if not nested:
                            break
                        _, a, b = nested[0]
                        expanded = expand_macro(arms, expanded[a + 1:b])
                        if expanded is None:
                            break
                before = len(operations)
                if expanded:
                    record(expanded, ts[start].start, name + "!")
                if len(operations) == before:
                    unresolved.append({**location(path, source, ts[start].start), "reason": "unsupported operation wrapper", "text": source[ts[start].start:ts[end].end]})
                else:
                    regions.append((ts[start].start, ts[end].end))
        # The old op_def bindings map is an explicit alias-to-qualified-op map.
        for _, opening, end in invocations(ts, "op_def"):
            fs = fields(ts[opening + 1:end])
            binding = fs.get("bindings", [])
            if binding and binding[0].text == "{":
                for item in split(binding[1:-1]):
                    texts = [t.text for t in item]
                    if "=>" in texts:
                        arrow = texts.index("=>")
                        name = spelling(item[:arrow]).replace("::", ".")
                        if name.count(".") == 1 and arrow + 1 < len(item):
                            dialect, name = name.split(".")
                            body = tokens(f'name: "{name}", dialect: "{dialect}",')
                            op = operation(item[arrow + 1].text, body, path, source, item[0].start, "op_def!")
                            op["accessor_inventory"] = "external_cdsl_requires_inventory"
                            operations.append(op)
                            unresolved.append({**location(path, source, item[0].start),
                                               "reason": "op_def accessors require external CDSL inventory",
                                               "text": spelling(item)})
            regions.append((ts[opening - 2].start, ts[end].end))
        protected[path] = regions
    streams = {}
    for op in operations:
        if op["file"] not in streams:
            stream = tokens(files[op["file"]])
            streams[op["file"]] = stream, pairs(stream)
        stream, ps = streams[op["file"]]
        op.setdefault("impls", [])
        for at, token in enumerate(stream):
            if token.text != "impl":
                continue
            opening = at + 1
            while opening < len(stream) and stream[opening].text != "{":
                opening += 1
            header = stream[at + 1:opening]
            if header and header[-1].text == op["alias"] and opening in ps:
                close = ps[opening]
                op["impls"].append({"header": " ".join(t.text for t in header),
                                    "methods": [stream[j + 1].text for j in range(opening + 1, close - 1) if stream[j].text == "fn"],
                                    **location(op["file"], files[op["file"]], token.start)})
    operations.sort(key=lambda op: (op["file"], op["line"], op["alias"]))
    return operations, unresolved, protected


def rust_module(path):
    parts = list(Path(path).with_suffix("").parts)
    parts = parts[parts.index("src") + 1:] if "src" in parts else parts
    if parts and parts[-1] in ("mod", "lib", "main"):
        parts.pop()
    return "::".join(["crate"] + parts)


def callback_inventory(files, operations):
    callbacks, unresolved = [], []
    aliases = collections.defaultdict(list)
    for op in operations:
        aliases[op["alias"]].append(op)

    def record(header, methods, path, loc, known=None):
        texts = [token.text for token in header]
        if "for" not in texts or not texts:
            return
        at = texts.index("for")
        trait, target = header[:at], header[at + 1:]
        if not target or target[-1].text not in aliases:
            return
        alias = target[-1].text
        candidates = [known] if known else aliases[alias]
        prefix = [token.text for token in target[:-1] if token.text != "::"]
        if not prefix:
            imported = [target for local, target in use_paths(tokens(files[path])) if local == alias]
            if len(imported) == 1:
                prefix = list(imported[0][:-1])
        if not known:
            candidates = [op for op in candidates if op["file"] == path or op["dialect"] in prefix or Path(op["file"]).stem in prefix]
        if len(candidates) != 1 or not trait or any(token.text in ("$", "<", ">", ",") for token in trait):
            unresolved.append({**loc, "reason": "interface callback binding requires manual resolution", "text": " ".join(texts)})
            return
        op = candidates[0]
        trait_name = trait[-1].text
        dictionary = "__TIR_IFACE_" + alias + "_" + trait_name
        module = rust_module(path)
        callbacks.append({**loc, "operation": op["operation"], "alias": alias,
                          "trait": spelling(trait), "dictionary": dictionary,
                          "module": module, "path": module + "::" + dictionary,
                          "receiver": "&tir::OpHandle", "methods": methods})

    for path, source in files.items():
        stream = tokens(source)
        ps = pairs(stream)
        for at, token in enumerate(stream):
            if token.text != "impl":
                continue
            opening = at + 1
            while opening < len(stream) and stream[opening].text != "{":
                opening += 1
            if opening in ps:
                methods = [stream[j + 1].text for j in range(opening + 1, ps[opening] - 1) if stream[j].text == "fn"]
                record(stream[at + 1:opening], methods, path, location(path, source, token.start))
    for op in operations:
        for impl in op.get("impls", []):
            if "file" not in impl:
                loc = {key: op[key] for key in ("file", "line", "column")}
                record(tokens(impl["header"]), impl["methods"], op["file"], loc, op)
    callbacks.sort(key=lambda item: (item["file"], item["line"], item["dictionary"]))
    return callbacks, unresolved


def use_paths(ts):
    result = []
    def walk(body, prefix=()):
        for part in split(body):
            texts = [t.text for t in part]
            if "{" in texts:
                at = texts.index("{")
                walk(part[at + 1:-1], prefix + tuple(t for t in texts[:at] if t != "::"))
            else:
                plain = [t for t in texts if t != "::"]
                if "as" in plain:
                    at = plain.index("as")
                    result.append((plain[at + 1], prefix + tuple(plain[:at])))
                elif plain:
                    result.append((plain[-1], prefix + tuple(plain)))
    ps = pairs(ts)
    i = 0
    while i < len(ts):
        if ts[i].text == "use":
            end = i + 1
            while end < len(ts) and ts[end].text != ";":
                end = ps[end] + 1 if end in ps else end + 1
            walk(ts[i + 1:end])
            i = end
        i += 1
    return result


def rewrite(path, source, operations, protected):
    ts, edits, sites = tokens(source), [], []
    imports = use_paths(ts)
    import_regions = []
    delimiter_pairs = pairs(ts)
    for start, token in enumerate(ts):
        if token.text == "use":
            end = start + 1
            while end < len(ts) and ts[end].text != ";":
                end = delimiter_pairs[end] + 1 if end in delimiter_pairs else end + 1
            if end < len(ts):
                import_regions.append((ts[start].start, ts[end].end))
    by_alias = collections.defaultdict(list)
    for op in operations:
        by_alias[op["alias"]].append(op)
    for alias, target in imports:
        if target[-1] in by_alias and alias != target[-1]:
            by_alias[alias] = by_alias[target[-1]]
    method_owners = collections.defaultdict(set)
    for op in operations:
        for method in [accessor["method"] for accessor in op["accessors"]] + [method for impl in op.get("impls", []) for method in impl["methods"]]:
            method_owners[method].add(op["operation"])
    def resolve(type_tokens):
        parts = [t.text for t in type_tokens if t.text != "::"]
        if not parts:
            return None
        local, prefix = parts[-1], parts[:-1]
        candidates = by_alias.get(local, [])
        if not prefix:
            bindings = [(alias, target) for alias, target in imports if alias == local]
            if len(bindings) == 1:
                target = bindings[0][1]
                candidates = by_alias.get(target[-1], [])
                prefix = list(target[:-1])
            else:
                candidates = [op for op in candidates if op["file"] == path]
                if len(candidates) == 1:
                    return candidates[0], ""
                return None
        if len(candidates) > 1:
            candidates = [op for op in candidates if op["dialect"] in prefix or Path(op["file"]).stem in prefix]
        candidates = [op for op in candidates
                      if prefix[-1] in (op["dialect"], Path(op["file"]).stem)
                      or (prefix[-1] == "ops" and op["dialect"] in prefix)]
        if len(candidates) == 1:
            return candidates[0], "::".join(prefix) + "::"
        return None
    def report(i, reason, end=None):
        sites.append({**location(path, source, ts[i].start), "reason": reason,
                      "text": source[ts[i].start:ts[end if end is not None else i].end]})
    def add(start, end, text, category):
        edits.append((ts[start].start, ts[end].end, text, category))
    ps = pairs(ts)
    for i, token in enumerate(ts):
        if any(a <= token.start < b for a, b in protected):
            continue
        if any(a <= token.start < b for a, b in import_regions):
            if token.text in by_alias:
                report(i, "operation import requires removal after migration")
            continue
        if token.text == "impl":
            j = i + 1
            while j < len(ts) and ts[j].text not in ("{", ";"):
                j += 1
            if j < len(ts) and any(t.text in by_alias for t in ts[i:j]):
                report(i, "operation impl requires free handle callbacks", j)
                if j in ps:
                    sites[-1]["methods"] = [ts[k + 1].text for k in range(j + 1, ps[j] - 1) if ts[k].text == "fn"]
        if i + 4 < len(ts) and token.text in ("as_op", "is", "parse_ir", "of", "of_operation") and spelling(ts[i + 1:i + 3]) == "::<":
            end = i + 3
            while end < len(ts) and ts[end].text not in (">", ";", "{"):
                end += 1
            if end + 1 >= len(ts) or ts[end].text != ">" or ts[end + 1].text != "(":
                continue
            close = ps[end + 1]
            resolved = resolve(ts[i + 3:end])
            if not resolved:
                if any(t.text.endswith("Op") for t in ts[i + 3:end]):
                    report(i, "operation alias cannot be resolved", close)
                continue
            op, prefix = resolved
            descriptor = prefix + op["descriptor"]
            if token.text in ("as_op", "is") and i and ts[i - 1].text == "." and close == end + 2:
                add(i, close, ("as_operation" if token.text == "as_op" else "is_operation") + "(&" + descriptor + ")", "operation_query")
            elif token.text == "parse_ir":
                args = split(ts[end + 2:close])
                if len(args) == 2:
                    start = ts[i].start
                    head = source[start:ts[end + 1].end].replace(source[start:ts[end].end], "parse_operation")
                    arguments = source[args[0][0].start:args[-1][-1].end]
                    add(i, close, head + arguments + ", &" + descriptor + ")", "parse_operation")
                else:
                    report(i, "parse_ir does not have two arguments", close)
            elif token.text in ("of", "of_operation") and i >= 2 and ts[i - 1].text == "::" and ts[i - 2].text in ("OperationName", "DialectName"):
                start = i - 2
                while start >= 2 and ts[start - 1].text == "::":
                    start -= 2
                add(start, close, descriptor + (".operation_name()" if token.text == "of" else ".dialect_name()"), "static_name")
        if token.text in by_alias and i + 2 < len(ts) and ts[i + 1].text == "::":
            start = i
            while start >= 2 and ts[start - 1].text == "::":
                start -= 2
            if ts[i + 2].text == "new":
                resolved = resolve(ts[start:i + 1])
                if resolved:
                    op, prefix = resolved
                    add(start, i + 2, prefix + op["constructor"], "constructor")
                else:
                    report(start, "constructor alias cannot be resolved", i + 2)
            else:
                report(start, "associated operation item requires migration", i + 2)
        container = i >= 2 and ts[i - 1].text == "<" and ts[i - 2].text in ("Vec", "Option", "Result", "Box")
        if token.text in by_alias and i and (ts[i - 1].text in ("&", ":", "->") or container):
            # Bare names in imports, declarations, impls and macro arguments are
            # intentionally excluded. Qualified types need Rust name resolution.
            statement = i - 1
            while statement and ts[statement].text not in (";", "{", "}"):
                statement -= 1
            before = [t.text for t in ts[statement:i]]
            if not any(t in before for t in ("use", "impl", "!")) and not (i + 1 < len(ts) and ts[i + 1].text == "::"):
                if resolve([token]):
                    add(i, i, ("crate" if path.startswith("core/src/") else "tir") + "::OpHandle", "type")
                else:
                    report(i, "type alias cannot be resolved")
        if i + 2 < len(ts) and token.text == "." and ts[i + 2].text == "(":
            name = ts[i + 1].text
            candidates = sorted(method_owners[name])
            if candidates:
                sites.append({**location(path, source, ts[i + 1].start), "reason": "receiver method requires type evidence", "method": name, "candidate_operations": candidates})
    # Outer static-call replacements consume inner type sites. Reject all other
    # overlaps instead of depending on replacement order.
    edits.sort(key=lambda edit: (edit[0], -edit[1]))
    accepted = []
    for edit in edits:
        if accepted and edit[0] < accepted[-1][1]:
            if edit[1] <= accepted[-1][1]:
                continue
            raise ValueError(f"overlapping edits in {path} at byte {edit[0]}")
        accepted.append(edit)
    reported = {(site["line"], site["column"]) for site in sites}
    for i, token in enumerate(ts):
        if token.text not in by_alias:
            continue
        if any(a <= token.start < b for a, b in protected + import_regions):
            continue
        if any(a <= token.start < b for a, b, _, _ in accepted):
            continue
        loc = location(path, source, token.start)
        if (loc["line"], loc["column"]) not in reported:
            report(i, "operation alias requires manual migration")
    output = source
    for start, end, replacement, _ in reversed(accepted):
        output = output[:start] + replacement + output[end:]
    return output, accepted, sites


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("root", type=Path)
    parser.add_argument("--manifest", type=Path, required=True)
    parser.add_argument("--report", type=Path, required=True)
    parser.add_argument("--output-dir", type=Path)
    parser.add_argument("--read-manifest", type=Path, help="reuse a frozen alias inventory after declaration migration")
    args = parser.parse_args()
    root = args.root.resolve()
    output = args.output_dir.resolve() if args.output_dir else None
    if output and (output == root or root in output.parents):
        parser.error("--output-dir must be outside the input tree")
    for artifact in (args.manifest.resolve(), args.report.resolve()):
        if artifact == root or root in artifact.parents:
            parser.error("manifest and report must be outside the input tree")
    paths = sorted(p for p in root.rglob("*.rs") if not any(part in ("target", ".git", ".worktrees") for part in p.relative_to(root).parts))
    files = {p.relative_to(root).as_posix(): p.read_text() for p in paths}
    operations, unresolved, protected = inventory(files)
    if args.read_manifest:
        frozen = json.loads(args.read_manifest.read_text())
        if frozen.get("version") != 1 or not isinstance(frozen.get("operations"), list):
            parser.error("unsupported frozen manifest")
        operations = frozen["operations"]
    callbacks, callback_unresolved = callback_inventory(files, operations)
    unresolved.extend(callback_unresolved)
    counts = collections.Counter()
    changed = []
    for path, source in files.items():
        converted, edits, sites = rewrite(path, source, operations, protected[path])
        unresolved.extend(sites)
        counts.update(edit[3] for edit in edits)
        if edits:
            changed.append({"file": path, "edits": len(edits)})
        if output:
            destination = output / path
            destination.parent.mkdir(parents=True, exist_ok=True)
            destination.write_text(converted)
    manifest = {"version": 1, "operations": operations, "callbacks": callbacks}
    report = {"version": 1, "source_files": len(files), "operations": len(operations),
              "rewrites": dict(sorted(counts.items())), "changed_files": changed,
              "unresolved": sorted(unresolved, key=lambda site: (site["file"], site["line"], site["column"], site["reason"]))}
    for destination, data in ((args.manifest, manifest), (args.report, report)):
        destination.parent.mkdir(parents=True, exist_ok=True)
        destination.write_text(json.dumps(data, indent=2, sort_keys=True) + "\n")
    print(json.dumps({"source_files": len(files), "operations": len(operations), "rewrites": dict(counts), "unresolved": len(unresolved)}, sort_keys=True))


if __name__ == "__main__":
    main()
