function run() {
  const { other } = require('./b');
  return other();
}

module.exports = { run };
