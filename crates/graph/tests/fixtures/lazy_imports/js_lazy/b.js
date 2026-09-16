const { run } = require('./a');

function other() {
  return run;
}

module.exports = { other };
