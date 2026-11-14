const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const { execFileSync } = require('node:child_process');

const input = name => (process.env[`INPUT_${name.toUpperCase().replace(/ /g, '_')}`] || '').trim();
const lines = text => text.split('\n').map(line => line.trim()).filter(Boolean);
const readJson = file => JSON.parse(fs.readFileSync(file, 'utf8'));
const output = (command, args, options = {}) =>
  execFileSync(command, args, { ...options, encoding: 'utf8' }).trimEnd();

function files(directory, name) {
  return fs.readdirSync(directory, { withFileTypes: true }).flatMap(entry => {
    const file = path.join(directory, entry.name);
    if (entry.isDirectory()) return files(file, name);
    return entry.isFile() && entry.name === name && path.basename(directory) === 'new' ? [file] : [];
  });
}

function report(measurements, format, label, subject, architecture, environment) {
  const results = {};
  const variants = {};
  for (const measurement of measurements) {
    for (const [name, value] of Object.entries(measurement)) {
      const samples = value.latency.samples;
      if (!Array.isArray(samples) || !samples.length) throw new Error(`No samples for ${name}`);
      let id = `${architecture}/${name}`;
      if (format === 'bencher' && subject) {
        const [compiler, , optimization, stage, ...source] = name.split('/');
        if (!compiler || !optimization || !stage) {
          throw new Error(`Invalid comparison benchmark name: ${name}`);
        }
        const level = optimization.replace(/^-/, '');
        const sourceName = source.join('/').replace(/\.[^./]+$/, '');
        const phase = stage + (sourceName ? `/${sourceName}` : '');
        id = `${architecture}/${compiler}/${label}/source/${level}/${phase}`;
        variants[id] = {
          benchmark: `${label}/${level}${sourceName ? `/${sourceName}` : ''}`,
          group: `${{ compile: 'Compile', link: 'Link', run: 'Run' }[stage] || ''} on ${architecture}`,
          variant: compiler,
          subject: compiler === subject,
        };
      }
      if (Object.hasOwn(results, id)) throw new Error(`Duplicate benchmark measurement: ${id}`);
      const sorted = [...samples].sort((a, b) => a - b);
      const median = (sorted[Math.floor((sorted.length - 1) / 2)] + sorted[Math.floor(sorted.length / 2)]) / 2;
      results[id] = { latency: { value: median, samples } };
    }
  }
  if (!Object.keys(results).length) throw new Error('No benchmark measurements');
  return { metrics: [{ key: 'latency', label: 'Wall time', unit: 'ns' }], environment, results, variants };
}

try {
  process.chdir(process.env.GITHUB_WORKSPACE);
  process.env.LC_ALL = 'C';
  const label = input('label');
  const format = input('format') || 'criterion';
  const packageName = input('package');
  const benchmark = input('benchmark');
  const cargoArgs = lines(input('cargo-args') || 'bench');
  const excluded = new Set(lines(input('exclude')));
  if (!label || ['.', '..'].includes(label) || path.basename(label) !== label ||
      !['criterion', 'bencher'].includes(format)) {
    throw new Error('A label and valid measurement format are required');
  }

  const architecture = os.machine();
  const environment = `${architecture} ${os.cpus()[0]?.model.trim() || ''}`;
  let libc = `glibc ${process.report.getReport().header.glibcVersionRuntime || 'unknown'}`;
  try { libc = output('ldd', ['--version']).split('\n')[0]; } catch { /* ldd is optional */ }
  fs.mkdirSync('bench-results', { recursive: true });
  fs.writeFileSync('bench-results/environment', `${architecture}: ${output('rustc', ['--version'])}; ${libc}\n`);

  let targets;
  if (packageName && benchmark) {
    targets = [{ packageName, benchmark }];
  } else {
    const metadata = JSON.parse(output('cargo', ['metadata', '--no-deps', '--format-version', '1']));
    targets = metadata.packages.flatMap(pkg => pkg.targets
      .filter(target => target.kind.includes('bench'))
      .filter(target => !packageName || pkg.name === packageName)
      .filter(target => !benchmark || target.name === benchmark)
      .filter(target => !excluded.has(`${pkg.name}/${target.name}`))
      .map(target => ({ packageName: pkg.name, benchmark: target.name })));
  }
  if (!targets.length) throw new Error('No benchmark targets selected');

  for (const target of targets) {
    const bundle = path.join('bench-results', label, `${target.packageName}-${target.benchmark}`);
    fs.rmSync(bundle, { recursive: true, force: true });
    const criterion = path.resolve(bundle, 'criterion');
    const env = { ...process.env, CRITERION_HOME: criterion };
    execFileSync('cargo', [...cargoArgs, '-p', target.packageName, '--bench', target.benchmark], { env, stdio: 'inherit' });

    let measurements;
    if (format === 'criterion') {
      const listed = output('cargo', ['bench', '--locked', '-p', target.packageName, '--bench', target.benchmark, '--', '--list'], { env })
        .split('\n').filter(line => line.endsWith(': benchmark'))
        .map(line => line.slice(0, -': benchmark'.length)).sort();
      const recorded = files(criterion, 'benchmark.json').map(file => readJson(file).full_id).sort();
      fs.writeFileSync(path.join(bundle, 'listed'), listed.map(name => `${name}\n`).join(''));
      fs.writeFileSync(path.join(bundle, 'recorded'), recorded.map(name => `${name}\n`).join(''));
      if (!listed.length || JSON.stringify(listed) !== JSON.stringify(recorded)) {
        throw new Error(`Benchmark results do not match the listed benchmarks in ${bundle}`);
      }
      measurements = files(criterion, 'sample.json').map(file => {
        const { times, iters } = readJson(file);
        const { full_id: name } = readJson(path.join(path.dirname(file), 'benchmark.json'));
        return { [name]: { latency: { samples: times.map((time, index) => time / iters[index]) } } };
      });
    } else {
      measurements = files(criterion, 'bencher.json').map(readJson);
    }
    const summary = report(measurements, format, label, input('subject'), architecture, environment);
    const temporary = path.join(bundle, 'summary.tmp');
    fs.writeFileSync(temporary, `${JSON.stringify(summary, null, 2)}\n`);
    fs.renameSync(temporary, path.join(bundle, 'summary.json'));
  }
} catch (error) {
  console.error(`::error::${error.message}`);
  process.exitCode = 1;
}
