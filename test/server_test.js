const http = require('http');
const WebSocket = require('ws');
// const url = require('url');
const fs = require('fs');
const path = require('path');

const MIME = {
	'.js': 'text/javascript',
	'.css': 'text/css',
	'.html': 'text/html',
	'.wasm': 'application/wasm',
}

const server = http.createServer((req, res) => {
	let { method, url } = req;
	console.log(method, url, req.headers);
	if (method == 'GET' && url == '/')
		url = "index.html"
	let contentType = 'text/html';
	contentType = (MIME[path.extname(url)] || 'text/html')
	if (method == 'GET') {
		const filePath = __dirname + '/../pkg/' + url;
		console.log(filePath)
		try {
			const size = fs.statSync(filePath).size;
			res.writeHead(200, { 'Content-Type': contentType, 'Content-Length': size });
			let readStream = fs.createReadStream(filePath);
			readStream.pipe(res);
		} catch (e) {
			console.error(e);
			res.writeHead(404).end("error");
		}
	}
	else
		res.writeHead(404).end("error");
});

const wss = new WebSocket.Server({ server });

wss.on('connection', (ws) => {
	console.log("new connection")
	ws.on('message', function incoming(message) {
		console.log('received: %s', message);
		ws.send('pong');
	});
})
console.log("Listening on 8080");
server.listen(8080);
