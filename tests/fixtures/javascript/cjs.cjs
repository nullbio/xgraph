const fs = require('node:fs');
const { readFileSync } = require('node:fs');

function readConfig(path) {
    return readFileSync(path, 'utf8');
}

module.exports = readConfig;
exports.fs = fs;
