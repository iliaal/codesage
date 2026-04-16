package main

import (
	"fmt"
	"net/http"
)

const MaxRetries = 3

const (
	DefaultPort = 8080
	DefaultHost = "localhost"
)

type Config struct {
	Debug bool
	Name  string
	Port  int
}

type Handler interface {
	ServeHTTP(w http.ResponseWriter, r *http.Request)
	Shutdown() error
}

type Duration = int64

func NewConfig(name string) *Config {
	return &Config{
		Name: name,
		Port: DefaultPort,
	}
}

func (c *Config) String() string {
	return fmt.Sprintf("%s:%d", c.Name, c.Port)
}

func (c *Config) WithDebug() *Config {
	c.Debug = true
	return c
}

type Server struct {
	config *Config
}

func (s Server) Start() error {
	fmt.Println("starting", s.config.String())
	return http.ListenAndServe(fmt.Sprintf(":%d", s.config.Port), nil)
}

func process(config *Config) error {
	fmt.Println("processing", config.Name)
	return nil
}
