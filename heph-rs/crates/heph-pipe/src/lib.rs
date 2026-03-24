//! Pipeline utilities for chaining operations

use std::sync::mpsc;
use std::thread;

pub struct Pipeline<T> {
    rx: mpsc::Receiver<T>,
}

impl<T: Send + 'static> Pipeline<T> {
    pub fn new<F>(f: F) -> Self
    where
        F: FnOnce(mpsc::Sender<T>) + Send + 'static,
    {
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || f(tx));
        Self { rx }
    }

    pub fn map<U, F>(self, f: F) -> Pipeline<U>
    where
        U: Send + 'static,
        F: Fn(T) -> U + Send + 'static,
    {
        Pipeline::new(move |tx| {
            for item in self.rx {
                let _ = tx.send(f(item));
            }
        })
    }

    pub fn filter<F>(self, f: F) -> Pipeline<T>
    where
        F: Fn(&T) -> bool + Send + 'static,
    {
        Pipeline::new(move |tx| {
            for item in self.rx {
                if f(&item) {
                    let _ = tx.send(item);
                }
            }
        })
    }

    pub fn collect(self) -> Vec<T> {
        self.rx.iter().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pipeline_map() {
        let pipe = Pipeline::new(|tx| {
            for i in 1..=5 {
                tx.send(i).unwrap();
            }
        });

        let result = pipe.map(|x| x * 2).collect();
        assert_eq!(result, vec![2, 4, 6, 8, 10]);
    }

    #[test]
    fn test_pipeline_filter() {
        let pipe = Pipeline::new(|tx| {
            for i in 1..=10 {
                tx.send(i).unwrap();
            }
        });

        let result = pipe.filter(|&x| x % 2 == 0).collect();
        assert_eq!(result, vec![2, 4, 6, 8, 10]);
    }

    #[test]
    fn test_pipeline_chain() {
        let pipe = Pipeline::new(|tx| {
            for i in 1..=10 {
                tx.send(i).unwrap();
            }
        });

        let result = pipe
            .filter(|&x| x > 3)
            .map(|x| x * 2)
            .collect();
        assert_eq!(result, vec![8, 10, 12, 14, 16, 18, 20]);
    }
}
