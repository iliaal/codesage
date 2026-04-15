const express = require('express');
const DEFAULT_PORT = 3000;

class Router {
  constructor(options) {
    this.options = options;
  }

  get(path, handler) {
    return this;
  }

  post(path, handler) {
    return this;
  }
}

function createApp(config) {
  const app = express();
  return app;
}

function middleware(req, res, next) {
  next();
}

module.exports = { Router, createApp, middleware, DEFAULT_PORT };
