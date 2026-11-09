#!/usr/bin/env python3
"""Capture authored type metadata, generate checked records and fenced previews.

Generation keeps Rust previews outside the repository. --apply is a separate,
source-hash-fenced step. --check compares artifacts; --verify-cdsl CDSL executes
every exported record through an existing cdslc, comparing all portable values
and host identities with the captured Rust metadata.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import tomllib

import migrate_operation_handles as rust
import migrate_dialects_to_cdsl as operations

ROOT = Path(__file__).resolve().parents[2]
OUT = ROOT / 'core/defs/types'
BASE_ORDER = ['INT', 'FLOAT', 'UNIT', 'TUPLE', 'STATE', 'NAT_ADD', 'RAW', 'ITER', 'SEM_STATE', 'NAMED']
CTOR_FIELDS = {'dialect', 'name', 'params', 'kind', 'validate', 'carrier'}
SPELLING_FIELDS = {'ctor', 'keys', 'parse', 'print', 'variadic_tail'}
LIBRARY = '''pub extern type TypeDefinitions;
pub enum Kind { Data, Ghost, Nat, Sym, Bool };
pub enum Param { One(Kind), Opt(Kind), List(Kind), Fields, Any };
pub extern type Terms;
pub extern type TypeArgs;
pub extern type Validation;
pub extern type Carrier;
pub extern type Context;
pub extern type CtorId;
pub extern type Mnemonic;
pub extern type Parser;
pub extern type ParseResult;
pub extern type Spellings;
pub extern type TypeId;
pub extern type Formatter;
pub extern type PrintResult;
pub type ConstructorSpec = Record<{
    dialect: String, name: String,
    params: List<Record<{ name: String, param: Param }>>,
    kind: Kind,
    validate: (terms: Terms, args: TypeArgs) -> Validation,
    carrier: (terms: Terms, args: TypeArgs) -> Carrier,
}>;
pub type SpellingSpec = Record<{
    ctor_id: (context: Context) -> CtorId,
    keys: List<String>,
    parse: (mnemonic: Mnemonic, parser: Parser, context: Context) -> ParseResult,
    print: (spellings: Spellings, ty: TypeId, formatter: Formatter) -> PrintResult,
    variadic_tail: Bool,
}>;
'''


def sha(source):
    return hashlib.sha256(source.encode()).hexdigest()


def excerpt(source, ts):
    return source[ts[0].start:ts[-1].end] if ts else ''


def fields(source, ts):
    result = {}
    for part in operations.split_fields(ts):
        if len(part) >= 3 and part[1].text == ':':
            result[part[0].text] = excerpt(source, part[2:])
        elif len(part) == 1:
            result[part[0].text] = part[0].text
    return result


def replace(source, edits):
    end = len(source)
    for edit in sorted(edits, key=lambda edit: edit['start'], reverse=True):
        if edit['end'] > end:
            raise ValueError('overlapping source ownership spans')
        source = source[:edit['start']] + edit['replacement'] + source[edit['end']:]
        end = edit['start']
    return source


def edition(path):
    for parent in path.parents:
        manifest = parent / 'Cargo.toml'
        if manifest.exists():
            package = tomllib.loads(manifest.read_text()).get('package', {})
            value = package.get('edition')
            if isinstance(value, str):
                return value
    return tomllib.loads((ROOT / 'Cargo.toml').read_text())['workspace']['package']['edition']


def formatted(source, path):
    return subprocess.run(['rustfmt', '--edition', edition(path), '--emit', 'stdout'],
                          input=source, text=True, capture_output=True, check=True).stdout


def prepare(source, path):
    """Expand only the existing finite metadata macros and constant factories."""
    ts = rust.tokens(source)
    ps = rust.pairs(ts)
    edits = []
    wrappers = {}
    for i in range(len(ts) - 3):
        if (ts[i].text, ts[i + 1].text) == ('macro_rules', '!') and ts[i + 2].text in ('scalar_type', 'unit_type'):
            end = ps[i + 3]
            name = ts[i + 2].text
            arms = rust.macro_arms(ts[i + 4:end])
            if len(arms) != 1:
                raise ValueError(f'{path}: non-single metadata macro {name}')
            wrappers[name] = arms[0][1]
            edits.append({'start': ts[i].start, 'end': ts[end].end, 'replacement': ''})
    for name, template in wrappers.items():
        keys = ['name', 'ctor', 'mnemonic', 'bits'] if name == 'scalar_type' else ['name', 'ctor', 'mnemonic', 'variadic_tail', 'doc']
        for start, opening, end in rust.invocations(ts, name):
            if any(edit['start'] <= ts[start].start < edit['end'] for edit in edits):
                continue
            args = rust.split(ts[opening + 1:end])
            if len(args) != len(keys):
                raise ValueError(f'{path}: unexpected {name} invocation')
            expanded = operations.text(operations.replace_template(template, dict(zip(keys, map(operations.text, args)))))
            last = end + int(end + 1 < len(ts) and ts[end + 1].text == ';')
            edits.append({'start': ts[start].start, 'end': ts[last].end, 'replacement': expanded})
    if path == 'core/src/dialects/layout.rs':
        factories = {}
        binary = None
        for i in range(len(ts) - 3):
            if ts[i].text == 'const' and ts[i + 1].text == 'fn' and ts[i + 2].text in ('region_value', 'nat'):
                opening = i + 3
                while ts[opening].text != '{':
                    opening += 1
                end = ps[opening]
                body = ts[opening + 1:end]
                at = next(j for j, token in enumerate(body) if token.text == 'TypeCtor')
                nested = rust.pairs(body)
                factories[ts[i + 2].text] = fields(source, body[at + 2:nested[at + 1]])
                edits.append({'start': ts[i].start, 'end': ts[end].end, 'replacement': ''})
            if ts[i].text == 'const' and ts[i + 1].text == 'BINARY':
                end = next(j for j in range(i, len(ts)) if ts[j].text == ';')
                equal = next(j for j in range(i, end) if ts[j].text == '=')
                binary = excerpt(source, ts[equal + 1:end])
                edits.append({'start': ts[i].start, 'end': ts[end].end, 'replacement': ''})
        for i in range(len(ts) - 2):
            if ts[i].text not in factories or ts[i + 1].text != '(' or ts[i - 1].text != '=':
                continue
            end = ps[i + 1]
            args = rust.split(ts[i + 2:end])
            fs = dict(factories[ts[i].text])
            fs['name'] = excerpt(source, args[0])
            if len(args) == 2:
                fs['params'] = binary if operations.compact(args[1]) == 'BINARY' else excerpt(source, args[1])
            expression = 'Ctor::new(TypeCtor {' + ', '.join(key + ': ' + fs[key] for key in ('dialect', 'name', 'params', 'kind', 'validate', 'carrier')) + '})'
            edits.append({'start': ts[i].start, 'end': ts[end].end, 'replacement': expression})
    return formatted(replace(source, edits), ROOT / path), edits


def owner_of(ts, ps, at):
    for i in range(at - 1, -1, -1):
        if ts[i].text == 'impl' and i + 2 in ps and ps[i + 2] > at:
            return ts[i + 1].text
    raise ValueError('spelling has no concrete impl owner')


def context_of(ts, ps, at):
    for i in range(at - 1, -1, -1):
        if ts[i].text == 'fn' and ts[i + 1].text == 'spelling' and i + 2 in ps:
            return ts[i + 3].text
    raise ValueError('spelling has no context parameter')


def capture():
    paths = sorted((ROOT / 'core/src/dialects').rglob('*.rs'))
    paths += [ROOT / path for path in ('core/src/backend/registers.rs', 'fcc/src/cir.rs', 'gpu/src/spirv/types.rs', 'libs/symbolic/src/term.rs')]
    result = []
    for path in paths:
        original = path.read_text()
        if not re.search(r'\b(TypeCtor|TypeSpelling)\s*\{', original):
            continue
        relative = str(path.relative_to(ROOT))
        if relative == 'libs/symbolic/src/term.rs':
            for index, name in enumerate(BASE_ORDER):
                if not re.search(r'pub const ' + name + r':\s*CtorId\s*=\s*CtorId\(' + str(index) + r'\)', original):
                    raise ValueError('fixed builtin constructor identity changed: ' + name)
        source, preparation = prepare(original, relative)
        ts = rust.tokens(source)
        ps = rust.pairs(ts)
        records = []
        base = 0
        for i in range(len(ts) - 1):
            if ts[i].text not in ('TypeCtor', 'TypeSpelling') or ts[i + 1].text != '{':
                continue
            end = ps[i + 1]
            fs = fields(source, ts[i + 2:end])
            expected = CTOR_FIELDS if ts[i].text == 'TypeCtor' else SPELLING_FIELDS
            if not fs:
                continue
            if set(fs) != expected:
                if ts[i - 1].text == 'struct':
                    continue
                raise ValueError(f'{relative}: unsupported {ts[i].text} fields: {sorted(fs)}')
            if ts[i].text == 'TypeCtor':
                if relative == 'libs/symbolic/src/term.rs':
                    name = 'base_' + BASE_ORDER[base]
                    base += 1
                else:
                    at = next(j for j in range(i - 1, -1, -1) if ts[j].text == 'static')
                    name = ts[at + 1].text
                owner = None
                context = None
            else:
                owner = owner_of(ts, ps, i)
                context = context_of(ts, ps, i)
                name = owner
            records.append({'sort': ts[i].text, 'name': name, 'owner': owner, 'context': context,
                            'start': ts[i].start, 'end': ts[end].end, 'fields': fs,
                            'payload_sha256': sha(source[ts[i].start:ts[end].end])})
        if records:
            if relative == 'libs/symbolic/src/term.rs' and base != len(BASE_ORDER):
                raise ValueError('fixed base constructor count changed')
            result.append({'path': relative, 'source': original, 'source_sha256': sha(original),
                           'canonical_sha256': sha(formatted(original, path)), 'prepared': source,
                           'preparation_spans': preparation, 'records': records})
    return {'version': 1, 'base_ctor_order': BASE_ORDER, 'files': result}


def enum_data(source, enum):
    ts = rust.tokens(source)
    if len(ts) < 3 or [token.text for token in ts[:2]] != [enum, '::']:
        raise ValueError('unexpected enum data ' + source)
    name = ts[2].text
    if enum == 'Kind':
        if len(ts) != 3 or name not in ('Data', 'Ghost', 'Nat', 'Sym', 'Bool'):
            raise ValueError('unexpected Kind ' + source)
        return f'Kind::{name}()', f'library::Kind::{name}()'
    if name in ('Fields', 'Any') and len(ts) == 3:
        return f'Param::{name}()', f'library::Param::{name}()'
    if name in ('One', 'Opt', 'List') and ts[3].text == '(' and ts[-1].text == ')':
        cdsl, value = enum_data(operations.text(ts[4:-1]), 'Kind')
        return f'Param::{name}({cdsl})', f'library::Param::{name}({value})'
    raise ValueError('unexpected Param ' + source)


def params_data(source):
    ts = rust.tokens(source)
    if [token.text for token in ts[:2]] != ['&', '['] or ts[-1].text != ']':
        raise ValueError('parameters must be a literal ordered slice: ' + source)
    cdsl, values = [], []
    for part in rust.split(ts[2:-1]):
        items = rust.split(part[1:-1])
        name = rust.literal(items[0])
        if name is None or len(items) != 2:
            raise ValueError('bad parameter entry')
        kind, value = enum_data(operations.text(items[1]), 'Param')
        cdsl.append('{ name: ' + json.dumps(name) + ', param: ' + kind + ' }')
        values.append('{name: ' + json.dumps(name) + ', param: ' + value + '}')
    return '[' + ', '.join(cdsl) + ']', '[' + ', '.join(values) + ']'


def qualify_self(source, owner):
    ts = rust.tokens(source)
    return replace(source, [{'start': token.start, 'end': token.end, 'replacement': owner}
                            for token in ts if token.text == 'Self'])


def remove_metadata_imports(source):
    """Drop only names made unused by replacing the captured metadata."""
    for name in ('Kind', 'Param', 'TypeCtor'):
        ts = rust.tokens(source)
        uses = []
        for start, token in enumerate(ts):
            if token.text == 'use':
                end = next(i for i in range(start, len(ts)) if ts[i].text == ';')
                uses.append((start, end))
        occurrences = [i for i, token in enumerate(ts) if token.text == name]
        if any(not any(start <= i <= end for start, end in uses) for i in occurrences):
            continue
        edits = []
        for i in occurrences:
            start, end = next((start, end) for start, end in uses if start <= i <= end)
            if not any(token.text == '{' for token in ts[start:end]):
                edits.append({'start': ts[start].start, 'end': ts[end].end, 'replacement': ''})
            elif ts[i + 1].text == ',':
                edits.append({'start': ts[i].start, 'end': ts[i + 1].end, 'replacement': ''})
            elif ts[i - 1].text == ',':
                edits.append({'start': ts[i - 1].start, 'end': ts[i].end, 'replacement': ''})
            else:
                # Retain uncommon single-element grouped imports for review.
                continue
        source = replace(source, edits)
    return source


def emit_file(file):
    path = ROOT / file['path']
    stem = file['path'].removesuffix('.rs').replace('/', '_')
    crate = next(parent for parent in path.parents if (parent / 'Cargo.toml').exists())
    root_file = os.path.relpath(OUT / (stem + '.cdsl'), crate)
    symbolic = file['path'] == 'libs/symbolic/src/term.rs'
    runtime = 'super' if symbolic else '::tir'
    header = 'mod library;\nuse library::*;\nextern type constructors = TypeDefinitions;\nextern type spellings = TypeDefinitions;\n'
    externs, records, edits, exports = [], [], [], []
    for record in file['records']:
        fs = record['fields']
        name = record['name']
        hosts = {}
        def callback(role, expression, cdsl_type):
            foreign = name + '_' + role
            externs.append('extern let ' + foreign + ': ' + cdsl_type + ';')
            if record['owner']:
                expression = qualify_self(expression, record['owner'])
            hosts[foreign] = expression
            return foreign, '<foreign main::' + foreign + '>'
        if record['sort'] == 'TypeCtor':
            params, expected_params = params_data(fs['params'])
            kind, expected_kind = enum_data(fs['kind'], 'Kind')
            dialect, type_name = json.loads(fs['dialect']), json.loads(fs['name'])
            callback_type = '(terms: Terms, args: TypeArgs) -> '
            validate, expected_validate = callback('validate', fs['validate'], callback_type + 'Validation')
            carrier, expected_carrier = callback('carrier', fs['carrier'], callback_type + 'Carrier')
            binding = 'constructors::' + name
            entries = [('dialect', json.dumps(dialect)), ('name', json.dumps(type_name)), ('params', params), ('kind', kind), ('validate', validate), ('carrier', carrier)]
            expected = [('dialect', json.dumps(dialect)), ('name', json.dumps(type_name)), ('params', expected_params), ('kind', expected_kind), ('validate', expected_validate), ('carrier', expected_carrier)]
            records.append('pub let ' + binding + ': ConstructorSpec = {\n    ' + ',\n    '.join(k + ': ' + v for k, v in entries) + ',\n};')
            exports.append({'binding': binding, 'rust_hosts': hosts, 'runtime': runtime,
                            'expected_value': 'Named(' + json.dumps('main::' + binding) + ', {' + ', '.join(k + ': ' + v for k, v in expected) + '})'})
            if symbolic:
                # A build script renders these entries in the original BASE array.
                replacement = 'include!(concat!(env!("OUT_DIR"), "/cdsl-type-' + name + '.rs"))'
            else:
                replacement = '::tir::helpers::type_ctor! { file: ' + json.dumps(root_file) + ', binding: ' + binding + ', hosts: { ' + ', '.join(k + ' => ' + v for k, v in hosts.items()) + ' } }'
        else:
            parse, expected_parse = callback('parse', fs['parse'], '(mnemonic: Mnemonic, parser: Parser, context: Context) -> ParseResult')
            print_, expected_print = callback('print', fs['print'], '(spellings: Spellings, ty: TypeId, formatter: Formatter) -> PrintResult')
            dynamic = record['owner'] == 'AggregateType'
            cases = ['Struct', 'Union', 'Variant'] if dynamic else [None]
            replacements = []
            for case in cases:
                case_name = name + ('_' + case if case else '')
                binding = 'spellings::' + case_name
                expression = case.upper() + '.id(context)' if case else fs['ctor']
                provider = case_name + '_ctor_id'
                externs.append('extern let ' + provider + ': (context: Context) -> CtorId;')
                argument = 'context' if any(token.text == 'context' for token in rust.tokens(expression)) else '_context'
                local_hosts = {**hosts, provider: '|' + argument + '| { ' + expression + ' }'}
                keys = [case.lower()] if case else json.loads(fs['keys'].replace('&', '', 1))
                variadic = fs['variadic_tail']
                if variadic not in ('true', 'false'):
                    raise ValueError('nonliteral variadic tail')
                entries = [('ctor_id', provider), ('keys', json.dumps(keys)), ('parse', parse), ('print', print_), ('variadic_tail', variadic)]
                records.append('pub let ' + binding + ': SpellingSpec = {\n    ' + ',\n    '.join(k + ': ' + v for k, v in entries) + ',\n};')
                expected = [('ctor_id', '<foreign main::' + provider + '>'), ('keys', '[' + ', '.join(map(json.dumps, keys)) + ']'), ('parse', expected_parse), ('print', expected_print), ('variadic_tail', variadic)]
                exports.append({'binding': binding, 'rust_hosts': local_hosts, 'runtime': runtime,
                                'expected_value': 'Named(' + json.dumps('main::' + binding) + ', {' + ', '.join(k + ': ' + v for k, v in expected) + '})'})
                expression = '::tir::helpers::type_spelling! { file: ' + json.dumps(root_file) + ', binding: ' + binding + ', context: ' + record['context'] + ', hosts: { ' + ', '.join(k + ' => ' + v for k, v in local_hosts.items()) + ' } }'
                replacements.append(('AggregateKind::' + case + ' => ' if case else '') + expression)
            replacement = 'match kind { ' + ', '.join(replacements) + ' }' if dynamic else replacements[0]
        edits.append({'start': record['start'], 'end': record['end'], 'replacement': replacement,
                      'payload_sha256': record['payload_sha256']})
    preview = formatted(remove_metadata_imports(replace(file['prepared'], edits)), path)
    return stem, header + '\n' + '\n'.join(externs) + '\n\n' + '\n\n'.join(records) + '\n', preview, exports, edits


def outputs(inventory, preview_dir):
    result = {OUT / 'library.cdsl': LIBRARY}
    manifest = {'base_ctor_order': inventory['base_ctor_order'], 'files': []}
    for file in inventory['files']:
        stem, cdsl, preview, exports, spans = emit_file(file)
        result[OUT / (stem + '.cdsl')] = cdsl
        result[preview_dir / file['path']] = preview
        manifest['files'].append({'source': file['path'], 'source_sha256': file['source_sha256'],
                                  'canonical_sha256': file['canonical_sha256'], 'cdsl': stem + '.cdsl',
                                  'preview_sha256': sha(preview), 'spans': spans, 'exports': exports})
    manifest['constructors'] = sum(export['binding'].startswith('constructors::') for file in manifest['files'] for export in file['exports'])
    manifest['spellings'] = sum(export['binding'].startswith('spellings::') for file in manifest['files'] for export in file['exports'])
    result[OUT / 'metadata-parity.json'] = json.dumps(manifest, indent=2) + '\n'
    return result, manifest


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--capture', action='store_true')
    parser.add_argument('--check', action='store_true')
    parser.add_argument('--apply', action='store_true')
    parser.add_argument('--verify-cdsl', type=Path, metavar='CDSLC')
    parser.add_argument('--preview-dir', type=Path, default=Path('/tmp/tir-type-metadata-previews'),
                        help='Rust preview directory outside the repository')
    args = parser.parse_args()
    preview_dir = args.preview_dir.resolve()
    if preview_dir.is_relative_to(ROOT):
        parser.error('--preview-dir must be outside the repository')
    inventory_path = OUT / 'inventory.json'
    if args.capture:
        if inventory_path.exists():
            raise SystemExit('refusing to overwrite the frozen source inventory')
        OUT.mkdir(parents=True, exist_ok=True)
        inventory_path.write_text(json.dumps(capture(), indent=2) + '\n')
    inventory = json.loads(inventory_path.read_text())
    artifacts, manifest = outputs(inventory, preview_dir)
    mismatches = []
    for path, contents in artifacts.items():
        if args.check:
            if not path.exists() or path.read_text() != contents:
                mismatches.append(str(path))
        else:
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(contents)
    if mismatches:
        raise SystemExit('artifact drift: ' + ', '.join(mismatches))
    if args.verify_cdsl:
        for file in manifest['files']:
            for export in file['exports']:
                process = subprocess.run([str(args.verify_cdsl.resolve()), 'eval', str(OUT / file['cdsl']), export['binding']], text=True, capture_output=True)
                if process.returncode or process.stdout.strip() != export['expected_value']:
                    raise SystemExit(file['cdsl'] + ' ' + export['binding'] + '\nexpected: ' + export['expected_value'] + '\nactual: ' + process.stdout + process.stderr)
    if args.apply:
        for file in inventory['files']:
            current = (ROOT / file['path']).read_text()
            preview = artifacts[preview_dir / file['path']]
            if current != preview and sha(formatted(current, ROOT / file['path'])) != file['canonical_sha256']:
                raise SystemExit('source fence changed: ' + file['path'])
        for file in inventory['files']:
            (ROOT / file['path']).write_text(artifacts[preview_dir / file['path']])
    print(f"{manifest['constructors']} constructors, {manifest['spellings']} spellings; {len(manifest['files'])} files")


if __name__ == '__main__':
    main()
