'use strict';
const http = require('http');
const inspector = require('inspector');
const counters = require(process.env.LIFETIME_ADDON);
const url = new URL(inspector.url());
function listing() {
  return new Promise((resolve, reject) => {
    http.get({host: '127.0.0.1', port: url.port, path: '/json/list', agent: false}, response => {
      let body = '';
      response.setEncoding('utf8');
      response.on('data', chunk => { body += chunk; });
      response.on('end', () => resolve(JSON.parse(body)));
      response.on('error', reject);
    }).on('error', reject);
  });
}
async function main() {
  for (let i = 0; i < 8; i++) await listing();
  const before = counters.outstanding();
  for (let i = 0; i < 64; i++) {
    const targets = await listing();
    if (targets.length) throw new Error('inspector UUID disclosed');
  }
  console.log(`COPIES ${counters.outstanding() - before}`);
}
main().catch(error => { console.error(error); process.exitCode = 1; });
