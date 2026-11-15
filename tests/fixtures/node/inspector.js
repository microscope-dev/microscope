


'use strict';
const http = require('http');
const inspector = require('inspector');

console.log(`ENV ${JSON.stringify(process.env.NODE_OPTIONS ?? null)} ${JSON.stringify(process.env.MICROSCOPE_TRACER ?? null)}`);
const url = inspector.url();
if (!url) {
  console.log('NO INSPECTOR');
} else {
  const { port, pathname } = new URL(url);
  console.log(`TARGET ${pathname.slice(1)}`);
  http.get({ host: '127.0.0.1', port, path: '/json/list' }, (response) => {
    let body = '';
    response.setEncoding('utf8');
    response.on('data', (chunk) => { body += chunk; });
    response.on('end', () => console.log(`LIST ${JSON.stringify(JSON.parse(body))}`));
  });
}
