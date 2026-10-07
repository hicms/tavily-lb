// Fake Tavily API used by the e2e suite. Behaviour is chosen by the pool key:
//   *revoked*   -> 401 (key becomes invalid)
//   *limited*   -> 429 (key cools down)
//   *exhausted* -> 432 (quota exhausted)
//   anything else succeeds. /usage reports usage=42, limit=1000.
import http from 'node:http';

const port = Number(process.env.MOCK_PORT ?? 18281);
let calls = [];
let seq = 0;

function send(res, status, body) {
  res.writeHead(status, {'content-type': 'application/json'});
  res.end(JSON.stringify(body));
}

const server = http.createServer((req, res) => {
  const chunks = [];
  req.on('data', (c) => chunks.push(c));
  req.on('end', () => {
    const url = new URL(req.url, `http://127.0.0.1:${port}`);
    const raw = Buffer.concat(chunks).toString('utf8');

    if (url.pathname === '/__calls') return send(res, 200, calls);
    if (url.pathname === '/__reset') {
      calls = [];
      return send(res, 200, {ok: true});
    }

    const key = (req.headers.authorization ?? '').replace(/^Bearer\s+/i, '');
    let body = null;
    try {
      body = raw ? JSON.parse(raw) : null;
    } catch {
      body = raw;
    }
    calls.push({method: req.method, path: url.pathname, key, body});

    if (key.includes('revoked')) return send(res, 401, {detail: {error: 'Unauthorized: missing or invalid API key.'}});
    if (key.includes('limited')) return send(res, 429, {detail: {error: 'Too many requests'}});
    if (key.includes('exhausted')) return send(res, 432, {detail: {error: 'Plan usage limit exceeded'}});

    const request_id = `mock-${++seq}`;
    switch (url.pathname) {
      case '/usage':
        return send(res, 200, {key: {usage: 42, limit: 1000}, account: {plan_limit: 1000}});
      case '/search':
        return send(res, 200, {
          query: body?.query,
          answer: null,
          results: [{title: `Mock result for ${body?.query}`, url: 'https://example.com/mock', content: 'mock', score: 0.9}],
          request_id,
        });
      case '/extract':
        return send(res, 200, {
          results: (body?.urls ?? []).map((u) => ({url: u, raw_content: `content of ${u}`})),
          failed_results: [],
          request_id,
        });
      case '/crawl':
        return send(res, 200, {base_url: body?.url, results: [{url: body?.url, raw_content: 'crawled'}], request_id});
      case '/map':
        return send(res, 200, {base_url: body?.url, results: [`${body?.url}/a`, `${body?.url}/b`], request_id});
      default:
        return send(res, 404, {detail: {error: 'not found'}});
    }
  });
});

server.listen(port, '127.0.0.1', () => {
  console.log(`mock upstream listening on ${port}`);
});
