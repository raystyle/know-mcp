// Package daemon demonstrates Go AST chunking in the smoke corpus:
// top-level funcs and type declarations are chunk boundaries.
package daemon

import (
	"fmt"
	"sync"
	"time"
)

// Supervisor holds worker state and guards shutdown.
type Supervisor struct {
	mu       sync.Mutex
	workers  map[int]*Worker
	shutdown chan struct{}
}

// Worker is one pipeline stage.
type Worker struct {
	ID    int
	Tasks chan string
}

// Start launches n workers draining the task channel.
func (s *Supervisor) Start(n int) error {
	s.mu.Lock()
	defer s.mu.Unlock()
	for i := 0; i < n; i++ {
		w := &Worker{ID: i, Tasks: make(chan string, 64)}
		s.workers[i] = w
		go s.run(w)
	}
	return nil
}

// run is the worker loop: drain tasks until shutdown closes.
func (s *Supervisor) run(w *Worker) {
	for {
		select {
		case task, ok := <-w.Tasks:
			if !ok {
				return
			}
			fmt.Printf("worker %d: %s\n", w.ID, task)
		case <-s.shutdown:
			return
		}
	}
}

// Stop closes the shutdown channel and waits for quiescence.
func (s *Supervisor) Stop(timeout time.Duration) error {
	close(s.shutdown)
	deadline := time.Now().Add(timeout)
	for time.Now().Before(deadline) {
		if len(s.workers) == 0 {
			return nil
		}
		time.Sleep(100 * time.Millisecond)
	}
	return fmt.Errorf("shutdown deadline exceeded after %v", timeout)
}
