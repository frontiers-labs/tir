#!/usr/bin/env python3
"""Convert the frozen Rust operation declarations to checked CDSL records.

`--capture` saves the exact source/declaration payload before the first conversion.
The default writes only declaration spans and generated recipe/model bindings.
`--check` compares all generated artifacts without changing them. The frozen
inventory permits deterministic checking after the old declarations are removed.
"""
import argparse
import hashlib
import json
import os
import re
import subprocess
import tomllib
from pathlib import Path

import migrate_operation_handles as rust

ROOT = Path(__file__).resolve().parents[2]
OUT = ROOT / 'core/defs/converted'
INVENTORY = OUT / 'inventory.json'
WRAPPERS = {'int_binop', 'vector_binop', 'arithmetic_op', 'arithmetic_conversion', 'to_integer', 'env_op'}


def sha(text):
    return hashlib.sha256(text.encode()).hexdigest()


def text(ts):
    """Keep joint Rust punctuation and literal suffixes joint, without crossing gaps."""
    result = []
    previous = None
    for token in ts:
        if previous is not None and previous.end != token.start:
            result.append(' ')
        result.append(token.text)
        previous = token
    return ''.join(result)


def compact(ts):
    return rust.spelling(ts)


def split_fields(ts):
    """Commas in Rust closure parameter lists do not delimit record fields."""
    ps = rust.pairs(ts)
    result, start, i, closure = [], 0, 0, False
    while i < len(ts):
        if i in ps:
            i = ps[i] + 1
            continue
        if ts[i].text == '|':
            closure = not closure
        elif ts[i].text == ',' and not closure:
            result.append(ts[start:i])
            start = i + 1
        i += 1
    if start < len(ts):
        result.append(ts[start:])
    return result


def rust_fields(ts):
    return {part[0].text: part[2:] for part in split_fields(ts)
            if len(part) > 2 and part[1].text == ':'}


def literal(ts):
    value = json.loads(ts[0].text, strict=False) if len(ts) == 1 and ts[0].text.startswith('"') else None
    if value is None:
        raise ValueError('expected literal: ' + text(ts))
    return value


def replace_template(ts, bindings):
    """Expand only the finite metadata wrappers captured by this migration."""
    ps = rust.pairs(ts)
    pieces, ordinary = [], []
    def flush():
        if ordinary:
            pieces.append(text(ordinary))
            ordinary.clear()
    i = 0
    while i < len(ts):
        if ts[i].text == '$' and i + 1 < len(ts):
            flush()
            if ts[i + 1].text == '(':
                end = ps[i + 1]
                at = end + 1
                separator = []
                while ts[at].text not in ('*', '+', '?'):
                    separator.append(ts[at])
                    at += 1
                inner = ts[i + 2:end]
                names = re.findall(r'\$\s*(\w+)', text(inner))
                sequences = [bindings[n] for n in names if n in bindings and isinstance(bindings[n], list)]
                length = len(sequences[0]) if sequences else 1
                if any(len(s) != length for s in sequences):
                    raise ValueError('wrapper repetition length mismatch')
                for index in range(length):
                    local = {n: v[index] if isinstance(v, list) else v for n, v in bindings.items()}
                    if index:
                        pieces.append(text(separator))
                    pieces.append(text(replace_template(inner, local)))
                i = at + 1
                continue
            name = ts[i + 1].text
            if name not in bindings:
                raise ValueError('unbound wrapper capture ' + name)
            value = bindings[name]
            if isinstance(value, list):
                raise ValueError('unexpanded wrapper repetition ' + name)
            pieces.append(value)
            i += 2
            continue
        ordinary.append(ts[i])
        i += 1
    flush()
    # Substitutions have independent offsets. Separate their serialized pieces
    # before retokenizing so accidental offset equality cannot join two origins.
    return rust.tokens(' '.join(pieces))


def expand_wrapper(name, args, macros):
    args = rust.split(args)
    if name == 'int_binop':
        op, opname, interfaces = args[:3]
        has_extra = args[3][0].text == '['
        extra = args[3][1:-1] if has_extra else []
        sem = args[4 if has_extra else 3]
        rest = args[5 if has_extra else 4:]
        fields = f'name:{text(opname)},dialect:"builtin",scheme:"forall W: nat. (int<W>, int<W>) -> int<W>",operands:O{{lhs,rhs}},results:R{{result}},interfaces:[{text(interfaces[1:-1])},{text(extra)}],sem:{text(sem)},splice_hooks:H{{zero:zero,width:width,smin:smin,ones:ones,nsw_flag:nsw_flag,nuw_flag:nuw_flag}},'
        fields += ','.join(text(part) for part in rest)
        return rust.tokens('operation!{' + text(op) + '{' + fields + '}}')
    if name == 'arithmetic_conversion':
        category = compact(args[0])
        scheme = '(float<_, _>) -> float<_, _>' if category == 'float' else '(int<_>) -> float<_, _>'
        width = 'float_width' if category == 'float' else 'int_width'
        args = [args[1], args[2], rust.tokens(json.dumps(scheme)), rust.tokens(width), *args[3:]]
        keys = ['op', 'name', 'scheme', 'source_width', 'kind', 'generic', 'rounded']
        template = macros[name][-1][1]
    elif name == 'env_op':
        keys = ['op', 'resource', 'name', 'access']
        bindings = {key: text(arg) for key, arg in zip(keys, args[:4])}
        ports = args[4]
        bindings.update(ports=ports[0].text, group=ports[2].text, port=ports[4].text,
                        scheme=text(args[5]))
        evaluate = args[6]
        ps = rust.pairs(evaluate)
        params = rust.split(evaluate[2:ps[1]])
        bindings.update(operands=text(params[0]), state=text(params[1]),
                        evaluate=text(evaluate[ps[1] + 1:]))
        return replace_template(macros[name][0][1], bindings)
    else:
        keys = {'vector_binop': ['op', 'name', 'sem'],
                'arithmetic_op': ['op', 'name', 'operand', 'scheme', 'kind', 'generic', 'rounded', 'arity'],
                'to_integer': ['op', 'name', 'kind', 'generic', 'rounded']}[name]
        template = macros[name][0][1]
    bindings = {key: text(arg) for key, arg in zip(keys, args)}
    if 'operand' in bindings:
        bindings['operand'] = [text(part) for part in rust.split(args[2][1:-1])]
    return replace_template(template, bindings)


def capture(files=None):
    files = files or sorted((ROOT / 'core/src/dialects').rglob('*.rs')) + [ROOT / 'core/src/backend/operations.rs']
    result = []
    for path in files:
        source = path.read_text()
        ts = rust.tokens(source)
        ps = rust.pairs(ts)
        macros, protected, spans, operations = {}, [], [], []
        for i in range(len(ts) - 3):
            if ts[i].text == 'macro_rules' and ts[i + 1].text == '!' and ts[i + 2].text in WRAPPERS:
                end = ps[i + 3]
                macros[ts[i + 2].text] = rust.macro_arms(ts[i + 4:end])
                protected.append((ts[i].start, ts[end].end))
                spans.append({'start': ts[i].start, 'end': ts[end].end, 'replacement': ''})
        candidates = []
        for name in ['operation', *sorted(macros)]:
            for start, opening, end in rust.invocations(ts, name):
                if any(a <= ts[start].start < b for a, b in protected):
                    continue
                stream = ts[start:end + 1] if name == 'operation' else expand_wrapper(name, ts[opening + 1:end], macros)
                invocations = list(rust.invocations(stream, 'operation'))
                if len(invocations) != 1:
                    raise ValueError(f'{path}: expected one declaration for {name}')
                _, a, b = invocations[0]
                body = stream[a + 1:b]
                parsed = rust_fields(body[2:-1])
                alias = body[0].text
                # A qualified tir::operation! includes the namespace in its replaced span.
                begin = ts[start].start
                if name == 'operation':
                    qualified = start
                    while qualified >= 2 and ts[qualified - 1].text == '::':
                        qualified -= 2
                    begin = ts[qualified].start
                finish = ts[end].end
                if end + 1 < len(ts) and ts[end + 1].text == ';':
                    finish = ts[end + 1].end
                candidates.append((begin, finish, alias, parsed))
        for begin, end, alias, fs in sorted(candidates):
            operations.append({'alias': alias, 'fields': {key: text(value) for key, value in fs.items()},
                               'declaration_sha256': sha(source[begin:end]),
                               'line': source.count('\n', 0, begin) + 1})
            spans.append({'start': begin, 'end': end, 'replacement': 'root' if len(operations) == 1 else ''})
        if operations:
            result.append({'path': str(path.relative_to(ROOT)), 'source_sha256': sha(source),
                           'source': source, 'spans': sorted(spans, key=lambda s: s['start']), 'operations': operations})
    return {'version': 1, 'files': result}


def group_names(value):
    ts = rust.tokens(value)
    return [part[0].text for part in rust.split(ts[2:-1])] if ts else []


def split_type(source):
    result, start, depth = [], 0, 0
    for i, ch in enumerate(source):
        depth += ch in '<('
        depth -= ch in '>)'
        if ch == ',' and depth == 0:
            result.append(source[start:i].strip())
            start = i + 1
    if source[start:].strip():
        result.append(source[start:].strip())
    return result


SIMPLE_SORTS = {'_': 'Any', 'int<_>': 'AnyInt', 'float<_,_>': 'AnyFloat',
                'ptr.p<_>': 'AnyPtr', 'fn<_,_>': 'AnyFn', 'tuple<_>': 'AnyTuple',
                'index': 'Index', 'son.control': 'Control', 'fp.env': 'FpEnv',
                'fp.rounding': 'FpRounding', 'layout.fields': 'LayoutFields', 'cir.va_list': 'VaList',
                'layout.type': 'LayoutType', 'backend.reg<_,_>': 'AnyReg', 'state<_>': 'AnyState'}


def sort_type(value, count=False):
    value = re.sub(r'\s+', '', value)
    if value.endswith('*'):
        return f'Many<{sort_type(value[:-1], count)}>'
    if value.endswith('?'):
        return f'Optional<{sort_type(value[:-1], count)}>'
    if count and value == 'C':
        return 'Count<C>'
    if value in SIMPLE_SORTS:
        return SIMPLE_SORTS[value]
    for old, new in [('int', 'Int'), ('float', 'Float'), ('vector.v', 'Vector'), ('ptr.p', 'Ptr'), ('state', 'State')]:
        if value.startswith(old + '<'):
            args = split_type(value[len(old) + 1:-1])
            return new + '<' + ', '.join(sort_type(a, count) for a in args) + '>'
    if re.fullmatch(r'[A-Z][A-Za-z0-9_]*|\d+', value):
        return value
    raise ValueError('unsupported source sort ' + value)


def scheme(value):
    source = literal(rust.tokens(value))
    generic = []
    count = False
    body = source
    if source.startswith('forall '):
        params, body = source[7:].split('. ', 1)
        for item in split_type(params):
            name, _, bound = item.partition(':')
            name, bound = name.strip(), bound.strip()
            count |= bound == 'index | int<_>'
            if bound not in ('', 'nat', 'index | int<_>'):
                raise ValueError('unsupported quantifier bound ' + item)
            generic.append(name + (': Nat' if bound == 'nat' else ''))
    lhs, rhs = body.split('->')
    lhs = lhs.strip()[1:-1]
    rhs = rhs.strip()
    if rhs.startswith('('):
        rhs = rhs[1:-1]
    return generic, [sort_type(v, count) for v in split_type(lhs)], [sort_type(v, count) for v in split_type(rhs)], count


def sexpr(source):
    ts = re.findall(r'\(|\)|[^\s()]+', source)
    def parse(index):
        if ts[index] != '(':
            return ts[index], index + 1
        result, index = [], index + 1
        while ts[index] != ')':
            item, index = parse(index)
            result.append(item)
        return result, index + 1
    result, end = parse(0)
    if end != len(ts):
        raise ValueError('trailing semantic expression')
    return result


def recipe(expression, operands, vocabulary):
    steps, splices = [], []
    def push(step):
        steps.append('tir::sem::SemRecipeStep::' + step)
        return len(steps) - 1
    def integer(width, value, signed):
        return push(f'Integer {{ width: {width}, value: {value}, signed: {str(signed).lower()} }}')
    def apply(kind, arguments):
        return push(f'Apply {{ kind: tir::sem::SymKind::{kind}, arguments: &[{", ".join(map(str, arguments))}] }}')
    def walk(expr, parameters=()):
        if isinstance(expr, str):
            if expr.startswith('$'):
                name = expr[1:]
                if name not in splices:
                    splices.append(name)
                return push('Splice(' + json.dumps(name) + ')')
            if expr in parameters:
                return push(f'Argument({parameters.index(expr)})')
            if expr in operands:
                return push(f'Operand({operands.index(expr)})')
            return integer(64, int(expr), True)
        op, *args = expr
        if len(args) == 1 and op == 'concat':
            return apply('IterConcat', [walk(args[0], parameters)])
        if len(args) == 1 and op in ('sext', 'zext', 'trunc', 'fptosi', 'fptoui'):
            arg = walk(args[0], parameters)
            width = push('ResultWidth { significant_bits: ' + str(op.startswith('fpto')).lower() + ', subtract: ' + str(int(op == 'trunc')) + ' }')
            if op == 'trunc':
                return apply('Extract', [arg, width, integer(16, 0, False)])
            return apply({'sext': 'SExt', 'zext': 'ZExt', 'fptosi': 'FPToSI', 'fptoui': 'FPToUI'}[op], [arg, width])
        if op in ('map', 'reduce'):
            iterator = walk(args[0], parameters)
            keyword, params, body = args[1]
            if keyword != 'lambda':
                raise ValueError('expected lambda')
            return apply(op.title(), [iterator, walk(body, params)])
        return apply(vocabulary[op], [walk(arg, parameters) for arg in args])
    root = walk(expression)
    return 'tir::sem::SemRecipe { steps: &[\n        ' + ',\n        '.join(steps) + f'\n    ], root: {root} }}', splices


def vocabulary():
    source = (ROOT / 'libs/symbolic/src/lang/sexpr.rs').read_text()
    result = dict(re.findall(r'\("(\w+)", SymKind::(\w+)\)', source))
    source = (ROOT / 'libs/symbolic/src/lang/ops.rs').read_text()
    result.update((name, kind) for kind, name in re.findall(r'op!\(\s*(\w+),\s*"(\w+)"', source))
    return result


def index_data(value):
    value = value.replace(' ', '')
    return '{ n: ' + str(value.startswith('n')).lower() + ', offset: ' + (value.split('+')[1] if '+' in value else ('0' if value == 'n' else value)) + ' }'


def slice_data(value):
    if value is None:
        return 'BindSlice::All()'
    if '..' not in value:
        return 'BindSlice::One(' + index_data(value) + ')'
    start, end = value.split('..')
    end = 'Optional::Some(' + index_data(end) + ')' if end else 'Optional::None()'
    return 'BindSlice::Range(' + index_data(start) + ', ' + end + ')'


def term_data(value):
    value = value.strip()
    match = re.fullmatch(r'(\w+)(?:\.(ports|results))?(?:\[(.*)\])?', value)
    if match:
        name, group, sliced = match.groups()
        if group:
            return 'BindTerm::Region({ name: ' + json.dumps(name) + ', ports: ' + str(group == 'ports').lower() + ', slice: ' + slice_data(sliced) + ' })'
        if name == 'results':
            return 'BindTerm::Results(' + slice_data(sliced) + ')'
        return 'BindTerm::Operands([' + json.dumps(name) + '])'
    if value.startswith('(') and value.endswith(')'):
        return 'BindTerm::Operands([' + ', '.join(json.dumps(v.strip()) for v in value[1:-1].split(',')) + '])'
    raise ValueError('unrecognized binding term ' + value)


def binding_data(value, counted):
    ts = rust.tokens(value)
    entries = []
    predicate = None
    for entry in rust.split(ts[2:-1]):
        name = entry[0].text
        payload = compact(entry[2:]) if len(entry) > 1 else name
        if name == 'predicate':
            predicate = term_data(payload)
        else:
            entries.append('{ name: ' + json.dumps(name) + ', terms: [' + ', '.join(term_data(part) for part in payload.split('~')) + '] }')
    result = '{ kind: BindKind::' + ts[0].text + '(), predicate: ' + predicate + ', chains: [' + ', '.join(entries) + ']'
    if counted:
        entries = rust.split(rust.tokens(counted)[1:-1])
        result += ', counted: { ' + ', '.join(text(e) if len(e) > 1 else e[0].text + ': ' + json.dumps(e[0].text) for e in entries) + ' }'
    return result + ' }'


ATTR_TYPES = {'Bool': 'Bool', 'Flag': 'Bool', 'Int': 'AttrInt', 'UInt': 'AttrUInt',
              'Str': 'String', 'Array': 'AttrArray', 'Dict': 'AttrDict', 'Type': 'AttrType',
              'Block': 'AttrBlock', 'FpSemantics': 'AttrFpSemantics', 'EvaluationContract': 'AttrEvaluationContract',
              'Float': 'AttrFloat', 'Predicate': 'AttrPredicate', 'Register': 'AttrRegister', 'any': 'AttrAny'}


def attribute_type(spelled):
    base = spelled.split(' in ')[0]
    if base.startswith('?'):
        return 'Optional<' + attribute_type(base[1:]) + '>'
    if base not in ATTR_TYPES:
        raise ValueError('unsupported attribute kind ' + base)
    return ATTR_TYPES[base]


def attributes(value):
    if not value:
        return []
    ts = rust.tokens(value)
    return [(part[0].text, literal(part[2:])) for part in rust.split(ts[2:-1])]


def callback_pairs(value):
    if not value:
        return []
    return [(text(pair[0]), text(pair[1])) for item in rust.split(rust.tokens(value)[1:-1])
            if (pair := rust.split(item[1:-1]))]


def without_operation_imports(source):
    stream = rust.tokens(source)
    changes = []
    for start, token in enumerate(stream):
        if token.text != 'use':
            continue
        end = start
        while end < len(stream) and stream[end].text != ';':
            end += 1
        body = stream[start + 1:end]
        candidates = [i for i, item in enumerate(body) if item.text == 'operation']
        for at in candidates:
            if not any(t.text == '{' for t in body):
                changes.append((token.start, stream[end].end, ''))
            elif at + 1 < len(body) and body[at + 1].text == ',':
                changes.append((body[at].start, body[at + 1].end, ''))
            elif at and body[at - 1].text == ',':
                changes.append((body[at - 1].start, body[at].end, ''))
            else:
                raise ValueError('unsupported grouped operation import')
    for start, end, replacement in reversed(changes):
        source = source[:start] + replacement + source[end:]
    return source


def semantic_host(name, program, build):
    source = ('fn model(operation: &tir::OpHandle, graph: &mut tir::sem::SemGraph) -> Option<tir::NodeId> {\n'
              f'    const RECIPE: tir::sem::SemRecipe = {program};\n    {build}\n}}')
    formatted = subprocess.run(['rustfmt', '--edition', '2024', '--emit', 'stdout'],
                               input=source, text=True, capture_output=True, check=True).stdout
    body = formatted[formatted.index('{'):].strip()
    return name + ' => |operation, graph| ' + body.replace('\n', '\n        ')


def node_callback_results(source):
    ts = rust.tokens(source)
    ps = rust.pairs(ts)
    results = {}
    for i in range(len(ts) - 2):
        if ts[i].text != 'fn' or ts[i + 2].text != '(':
            continue
        start = ps[i + 2] + 1
        end = start
        while end < len(ts) and ts[end].text != '{':
            end += 1
        result = compact(ts[start:end])
        if result in ('->Option<tir::NodeId>', '->Option<crate::NodeId>', '->Option<NodeId>'):
            results[ts[i + 1].text] = True
        elif result in ('->tir::NodeId', '->crate::NodeId', '->NodeId'):
            results[ts[i + 1].text] = False
    return results


def splice_call(callback, results):
    ts = rust.tokens(callback)
    if ts[0].text == '|':
        end = next(i for i in range(1, len(ts)) if ts[i].text == '|')
        if compact(ts[1:end]) != 'operation,graph':
            raise ValueError('unsupported splice closure parameters: ' + callback)
        call = text(ts[end + 1:])
        function = call[:call.index('(')].strip()
    else:
        function = compact(ts)
        call = '(' + callback + ')(operation, graph)'
    name = function.split('::')[-1]
    if name not in results:
        raise ValueError('splice callback has no captured NodeId return type: ' + function)
    return call if results[name] else call + '.into()'


def forwarding_callback(callback):
    match = re.fullmatch(r'\|([\w,\s]+)\|\s*([\w:]+)\(([\w,\s]*)\)', callback)
    if match and compact(rust.tokens(match[1])).rstrip(',') == compact(rust.tokens(match[3])).rstrip(','):
        return match[2]
    return callback


def emit_file(file, vocab, cases):
    path = Path(file['path'])
    stem = '_'.join(path.with_suffix('').parts[2:])
    externs, records, hosts = [], [], []
    node_results = node_callback_results(file['source'])
    namespaces = list(dict.fromkeys(literal(rust.tokens(op['fields']['dialect'])) for op in file['operations']))
    header = 'pub mod ops;\nmod library;\nuse ops::*;\nuse library::*;\n'
    header += ''.join('extern type ' + ('r#' + ns if ns in ('type', 'fn') else ns) + ' = Dialect;\n' for ns in namespaces)
    imports = dict(rust.use_paths(rust.tokens(file['source'])))
    names = [literal(rust.tokens(op['fields']['name'])) for op in file['operations']]
    if len(names) != len(set(names)):
        raise ValueError(file['path'] + ': duplicate local operation names require distinct Rust scopes')
    for operation in file['operations']:
        fs = operation['fields']
        namespace = literal(rust.tokens(fs['dialect']))
        ns = 'r#' + namespace if namespace in ('type', 'fn') else namespace
        name = literal(rust.tokens(fs['name']))
        symbol = rust.symbol(name)
        generics, ins, outs, count = scheme(fs['scheme'])
        input_names, output_names = group_names(fs.get('operands', '')), group_names(fs.get('results', ''))
        if len(ins) != len(input_names) or len(outs) != len(output_names):
            raise ValueError(f'{namespace}.{name}: scheme port mismatch')
        state = literal(rust.tokens(fs['state'])) if fs.get('state') else ''
        if state:
            generics.append('StateResources')
            if state in ('in', 'in_out'):
                input_names.append('state')
                ins.append('States<StateResources>')
            if state in ('out', 'in_out'):
                output_names.append('state')
                outs.append('States<StateResources>')
        generic_decl = '<' + ', '.join(generics) + '>' if generics else ''
        generic_use = '<' + ', '.join(g.split(':')[0] for g in generics) + '>' if generics else ''
        parameters = ', '.join(n + ': ' + t for n, t in zip(input_names, ins))
        result = '()' if not outs else outs[0] if len(outs) == 1 and output_names[0] == 'result' else '(' + ', '.join(n + ': ' + t for n, t in zip(output_names, outs)) + ',)'
        fields = ['signature: (' + parameters + ') -> ' + result]
        attrs = attributes(fs.get('attributes'))
        attr_fields = []
        attr_shape = []
        for attr, spelling in attrs:
            ty = attribute_type(spelling)
            codec = re.sub(r'[^a-zA-Z0-9]', '_', spelling.split(' in ')[0]).lower() + '_codec'
            attr_shape.append(attr + ': ' + ty)
            descriptor = '{ ty: ' + ty + ', codec: ' + codec
            if ' in ' in spelling:
                descriptor += ', vocabulary: ' + json.dumps(spelling.split(' in ')[1])
            descriptor += ' }'
            attr_fields.append(attr + ': ' + descriptor)
            cases['attributes'][spelling] = {'type': ty, 'codec': codec}
        if attr_fields:
            externs.append('type ' + symbol + '_attributes = Record<{ ' + ', '.join(attr_shape) + ' }>;')
            fields.append('attributes: { ' + ', '.join(attr_fields) + ' }')
        for role in ('sem', 'poison', 'requires'):
            if role not in fs:
                continue
            expression = sexpr(literal(rust.tokens(fs[role])))
            if role == 'sem':
                if not isinstance(expression, list) or expression[0] != 'set':
                    raise ValueError('semantic assignment expected')
                expression = expression[2]
            program, splices = recipe(expression, input_names[:len(ins) - int(state in ('in', 'in_out'))], vocab)
            hook_name = symbol + '_' + ('exec' if role == 'sem' else role) + '_model'
            expected_result = result if role == 'sem' else 'Bool'
            receiver = 'self: ' + symbol + '_attributes' if attrs else ''
            arguments = ', '.join(v for v in (receiver, parameters) if v)
            externs.append(f'extern fn {hook_name}{generic_decl}({arguments}) -> {expected_result};')
            fields.append(('exec' if role == 'sem' else role) + ': ' + hook_name + generic_use)
            hooks = rust_fields(rust.tokens(fs.get('splice_hooks', 'H {}'))[2:-1])
            splice_arms = []
            for splice in splices:
                if splice not in hooks:
                    raise ValueError(f'{namespace}.{name}: missing splice callback {splice}')
                callback = text(hooks[splice])
                splice_arms.append(json.dumps(splice) + ' => ' + splice_call(callback, node_results) + ',')
            arms = '\n                '.join(splice_arms)
            build = f'tir::sem::build_recipe_with(operation, graph, &RECIPE, |operation, name, graph| {{\n        match name {{\n                {arms}\n                _ => None,\n        }}\n    }})' if splices else 'tir::sem::build_defined_recipe(operation, graph, &RECIPE)'
            hosts.append(semantic_host(hook_name, program, build))
        if 'verify_hook' in fs:
            hook = symbol + '_verifier'
            externs.append(f'extern fn {hook}(operation: Operation, context: Context) -> Validation;')
            fields.append('verify: ' + hook)
            hosts.append(hook + ' => ' + forwarding_callback(fs['verify_hook']))
        interfaces = [text(part) for part in rust.split(rust.tokens(fs.get('interfaces', '[]'))[1:-1]) if part]
        callbacks = {compact(rust.tokens(trait)).split('::')[-1]: (trait, callback)
                     for trait, callback in callback_pairs(fs.get('interface_callbacks'))}
        interface_values = []
        for trait in interfaces:
            short = compact(rust.tokens(trait)).split('::')[-1]
            foreign = symbol + '_iface_' + short
            externs.append(f'extern let {foreign}: Interface;')
            interface_values.append(foreign)
            canonical = trait if '::' in trait else '::'.join(imports.get(trait, (trait,)))
            _, callback = callbacks.get(short, (canonical, '&' + canonical + 'Callbacks::DEFAULT'))
            if short not in callbacks:
                # A path ending in a trait gets its callback type in the same module.
                callback = '&' + compact(rust.tokens(canonical)) + 'Callbacks::DEFAULT'
            if short == 'Speculatable':
                callback = re.sub(r'Some\((\|[\w,\s]+\|\s*[\w:]+\([\w,\s]*\))\)',
                                  lambda match: 'Some(' + forwarding_callback(match[1]) + ')', callback)
            hosts.append(foreign + ' => (' + trait + ', ' + callback + ')')
        if interface_values:
            fields.append('interfaces: [' + ', '.join(interface_values) + ']')
        if 'regions' in fs:
            regions = rust_fields(rust.tokens(fs['regions'])[2:-1])
            output = []
            for region, descriptor in regions.items():
                desc = rust_fields(descriptor[2:-1])
                kind = compact(desc.get('kind', rust.tokens('Blocks')))
                many = compact(desc.get('variadic', rust.tokens('false')))
                output.append(region + ': { kind: RegionKind::' + kind + '(), variadic: ' + many + ' }')
            fields.append('regions: { ' + ', '.join(output) + ' }')
        if 'binds' in fs:
            fields.append('bindings: ' + binding_data(fs['binds'], fs.get('counted')))
        if 'print_hook' in fs:
            formatter, parser = symbol + '_printer', symbol + '_parser'
            externs.extend([f'extern fn {formatter}(operation: Operation, formatter: Formatter) -> PrintResult;',
                            f'extern fn {parser}(parser: Parser, context: Context, definition: Definition) -> ParseResult;'])
            hosts.extend([formatter + ' => ' + fs['print_hook'], parser + ' => ' + fs['parse_hook']])
            fields.append('assembly: { print: ' + formatter + ', parse: ' + parser + ' }')
        if fs.get('format') and 'print_hook' not in fs:
            raise ValueError(f'{namespace}.{name}: custom format has no callback')
        cases['sorts'].update({t: sort_type(t, count) for t in re.findall(r'(?:[\w.]+|_)(?:<[^<>]*>)?[?*]?', literal(rust.tokens(fs['scheme'])).split('. ', 1)[-1]) if t not in ('forall',) and not t.isdigit() and t != 'nat'})
        if count:
            cases['bounds']['Count<C>'] = 'forall C: index | int<_>'
        records.append(f'pub let {ns}::{"r#" + name if name in ("for", "yield", "loop", "switch", "return", "def", "fn", "type", "do", "while", "break", "continue") else name}{generic_decl} = {{\n    ' + ',\n    '.join(fields) + ',\n};')
    cdsl = header + '\n' + '\n'.join(externs) + '\n\n' + '\n\n'.join(records) + '\n'
    rust_block = ''
    crate = next(parent for parent in (ROOT / path).parents if (parent / 'Cargo.toml').exists())
    root_file = os.path.relpath(OUT / (stem + '.cdsl'), crate)
    rust_block += '\n\ntir::helpers::op_def! {\n    file: ' + json.dumps(root_file) + ',\n    hosts: {\n        ' + ',\n        '.join(hosts) + (',' if hosts else '') + '\n    },\n}\n'
    rust_block = subprocess.run(['rustfmt', '--edition', '2024', '--emit', 'stdout'],
                                input=rust_block, text=True, capture_output=True, check=True).stdout
    return stem, cdsl, rust_block


def library(cases):
    decls = ['use ::ops::*;', 'pub extern type Any;', 'pub extern type AnyInt;', 'pub extern type AnyFloat;',
             'pub extern type AnyPtr;', 'pub extern type AnyFn;', 'pub extern type AnyTuple;',
             'pub extern type Index;', 'pub extern type Control;', 'pub extern type FpEnv;',
             'pub extern type FpRounding;', 'pub extern type LayoutFields;', 'pub extern type LayoutType;',
             'pub extern type VaList;',
             'pub extern type AnyReg;', 'pub extern type AnyState;', 'pub extern type Count<C>;',
             'pub extern type Float<E: Nat, M: Nat>;', 'pub extern type Vector<N: Nat, T>;',
             'pub extern type Many<T>;', 'pub extern type State<R>;',
             'pub enum Optional<T> { None, Some(T) };',
             'pub extern type Formatter;', 'pub extern type PrintResult;', 'pub extern type Parser;',
             'pub extern type Definition;', 'pub extern type ParseResult;',
             'pub enum BindKind { Theta, Gamma };',
             'pub type BindIndex = Record<{ n: Bool, offset: Nat }>;',
             'pub enum BindSlice { All, One(BindIndex), Range(BindIndex, Optional<BindIndex>) };',
             'pub enum BindTerm { Operands(List<String>), Results(BindSlice), Region(Record<{ name: String, ports: Bool, slice: BindSlice }>) };']
    for ty in sorted(set(SIMPLE_SORTS.values())):
        decls.append('pub extern let ' + ty.lower() + '_sort: IrSort<' + ty + '>;')
    for ty, generics, arguments in [('Float', 'E: Nat, M: Nat', 'E, M'),
                                    ('Vector', 'N: Nat, T', 'N, T'),
                                    ('Many', 'T', 'T'), ('Optional', 'T', 'T'),
                                    ('Count', 'C', 'C'), ('State', 'R', 'R')]:
        decls.append('pub extern let ' + ty.lower() + '_sort<' + generics + '>: IrSort<' + ty + '<' + arguments + '>>;')
    for ty in sorted(set(ATTR_TYPES.values()) - {'Bool', 'String'}):
        if ty == 'AttrRegister' and not any(ty in data['type'] for data in cases['attributes'].values()):
            continue
        decls.append('pub extern type ' + ty + ';')
    seen_codecs = set()
    for kind, data in sorted(cases['attributes'].items()):
        if data['codec'] == 'flag_codec' or data['codec'] in seen_codecs:
            continue
        seen_codecs.add(data['codec'])
        decls.append('pub extern let ' + data['codec'] + ': Attribute<' + data['type'] + '>;')
    return '\n'.join(decls) + '\n'


def generated(inventory):
    vocab = vocabulary()
    cases = {'sorts': {}, 'attributes': {}, 'bounds': {}, 'bindings': 'BindKind / BindTerm / BindSlice / BindIndex ordinary data',
             'state_bundle': 'States<StateResources>, removed from value ports and retained as state ports',
             'models': 'typed flattened extern fn; Rust fn(&OpHandle,&mut SemGraph)->Option<NodeId>'}
    outputs = {}
    metadata = []
    for file in inventory['files']:
        stem, cdsl, block = emit_file(file, vocab, cases)
        outputs[OUT / (stem + '.cdsl')] = cdsl
        source = file['source']
        for span in reversed(file['spans']):
            replacement = block if span['replacement'] == 'root' else ''
            source = source[:span['start']] + replacement + source[span['end']:]
        source = without_operation_imports(source)
        outputs[ROOT / file['path']] = source
        metadata.extend({'file': file['path'], 'identity': literal(rust.tokens(op['fields']['dialect'])) + '.' + literal(rust.tokens(op['fields']['name'])),
                         'declaration_sha256': op['declaration_sha256'], 'fields_sha256': sha(json.dumps(op['fields'], sort_keys=True))}
                        for op in file['operations'])
    outputs[OUT / 'library.cdsl'] = library(cases)
    outputs[OUT / 'required-profile-cases.json'] = json.dumps(cases, indent=2, sort_keys=True) + '\n'
    outputs[OUT / 'metadata-parity.json'] = json.dumps({'operations': metadata, 'count': len(metadata),
          'source_contract': 'Every source field captured verbatim in inventory; generated signatures, codecs, callbacks, recipes, regions and binding data are derived from that payload.'}, indent=2, sort_keys=True) + '\n'
    return outputs


def canonical_rust(path, content):
    manifest = next((directory / 'Cargo.toml' for directory in [path.parent, *path.parents]
                     if (directory / 'Cargo.toml').is_file()), None)
    if manifest is None:
        raise ValueError('Rust source has no Cargo.toml: ' + str(path))
    data = tomllib.loads(manifest.read_text())
    edition = data.get('package', {}).get('edition', '2024')
    if isinstance(edition, dict):
        workspace = next((directory / 'Cargo.toml' for directory in manifest.parents
                          if (directory / 'Cargo.toml').is_file()
                          and 'workspace' in tomllib.loads((directory / 'Cargo.toml').read_text())), None)
        edition = tomllib.loads(workspace.read_text()).get('workspace', {}).get('package', {}).get('edition', '2024') if workspace else '2024'
    result = subprocess.run(['rustfmt', '--edition', str(edition), '--config', 'skip_children=true', '--emit', 'stdout'],
                            input=content, text=True, capture_output=True)
    if result.returncode:
        raise ValueError('rustfmt rejected generated source ' + str(path) + ': ' + result.stderr)
    return result.stdout

def main():
    global OUT, INVENTORY
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--capture', action='store_true')
    parser.add_argument('--check', action='store_true')
    parser.add_argument('--output-only', action='store_true', help='write CDSL and proof data; leave Rust untouched')
    parser.add_argument('--source', action='append', type=Path, help='capture only this Rust source; repeatable')
    parser.add_argument('--output-dir', type=Path, help='generated CDSL directory under core/defs')
    args = parser.parse_args()
    if args.output_dir:
        OUT = args.output_dir.resolve()
        INVENTORY = OUT / 'inventory.json'
    if args.capture:
        if INVENTORY.exists():
            raise SystemExit('inventory exists; refusing to overwrite the frozen payload')
        inventory = capture([p.resolve() for p in args.source] if args.source else None)
        OUT.mkdir(parents=True, exist_ok=True)
        INVENTORY.write_text(json.dumps(inventory, indent=2, sort_keys=True) + '\n')
    inventory = json.loads(INVENTORY.read_text())
    for file in inventory['files']:
        if sha(file['source']) != file['source_sha256']:
            raise SystemExit('frozen source hash mismatch: ' + file['path'])
    outputs = generated(inventory)
    outputs = {path: canonical_rust(path, content) if path.suffix == '.rs' else content
               for path, content in outputs.items()}
    differences = []
    for path, content in outputs.items():
        if args.output_only and path.suffix == '.rs':
            continue
        if path.exists() and path.read_text() == content:
            continue
        if args.check:
            differences.append(str(path.relative_to(ROOT)))
        else:
            # On first write only the frozen original or exact generated source is accepted.
            if path.suffix == '.rs' and path.read_text() != next(file['source'] for file in inventory['files'] if ROOT / file['path'] == path):
                if canonical_rust(path, path.read_text()) != content:
                    raise SystemExit('source changed since capture: ' + str(path))
            path.write_text(content)
    link = OUT / 'ops.cdsl'
    if not link.exists() and not args.check:
        link.symlink_to(os.path.relpath(ROOT / 'core/defs/ops.cdsl', OUT))
    if differences:
        raise SystemExit('generated artifacts differ:\n' + '\n'.join(differences))
    print(json.dumps({'files': len(inventory['files']), 'operations': sum(len(f['operations']) for f in inventory['files']), 'mode': 'check' if args.check else 'write'}, sort_keys=True))


if __name__ == '__main__':
    main()
