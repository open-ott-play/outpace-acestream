// Guarded Ubuntu mirror preparation shared with venus-os-ci-toolkit.
if (process.env.RUNNER_ENVIRONMENT !== 'github-hosted' || process.env.RUNNER_OS !== 'Linux') {
  console.log('Ubuntu mirror preparation does not apply to this runner');
  process.exit(0);
}
const fs = require('node:fs');
const mirrorFile = '/etc/apt/apt-mirrors.txt';
let stat;
try {
  stat = fs.lstatSync(mirrorFile);
} catch (error) {
  if (error.code !== 'ENOENT') throw error;
  console.log('No image-provisioned Ubuntu mirror list; leaving APT unchanged');
  process.exit(0);
}
if (!stat.isFile() || stat.isSymbolicLink()) throw new Error('Ubuntu mirror list is not a regular file');
const original = fs.readFileSync(mirrorFile);
const text = original.toString('utf8');
if (!Buffer.from(text).equals(original)) throw new Error('Ubuntu mirror list is not valid UTF-8');
const lines = text.match(/[^\n]*\n|[^\n]+$/g) || [];
const entries = lines.map((line, index) => ({index, fields: line.trim().split(/[ \t]+/)}))
  .filter(({fields}) => fields[0] && !fields[0].startsWith('#'));
const azure = entries.filter(({fields}) => fields[0] === 'http://azure.archive.ubuntu.com/ubuntu/');
if (!azure.length) {
  console.log('Azure Ubuntu mirror is not configured; leaving APT unchanged');
  process.exit(0);
}
const fallback = entries.filter(({fields}) => fields[0] === 'https://archive.ubuntu.com/ubuntu/');
if (azure.length !== 1 || azure[0].fields.length !== 2 || azure[0].fields[1] !== 'priority:1' ||
    fallback.length !== 1 || fallback[0].fields.length !== 2 || fallback[0].fields[1] !== 'priority:2') {
  console.log('::warning::Unrecognized Ubuntu mirror configuration; leaving APT unchanged');
  process.exit(0);
}
const updated = lines.filter((_, index) => index !== azure[0].index).join('');
if (!fs.readFileSync(mirrorFile).equals(original)) throw new Error('Ubuntu mirror list changed during preparation');
fs.writeFileSync(mirrorFile, updated);
console.log('Removed the Azure entry; retained the image-provisioned HTTPS Ubuntu fallback');
