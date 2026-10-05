;;; -*- lexical-binding: t -*-
;; Drive CIDER in batch Emacs. env: MOVA_ED_PKG (private package dir), NREPL_PORT, WORK_DIR.
(require 'package)
(setq package-user-dir (getenv "MOVA_ED_PKG"))
(setq package-archives nil)
(package-initialize)
(setq debug-on-error nil
      cider-repl-pop-to-buffer-on-connect nil
      cider-print-fn nil
      cider-show-error-buffer nil
      cider-auto-select-error-buffer nil
      cider-connection-message-fn nil
      cider-repl-display-help-banner nil
      cider-enrich-classpath nil
      cider-jack-in-dependency-injection-enabled nil
      cider-clojure-cli-global-options nil
      nrepl-log-messages nil
      cider-allow-jack-in-without-project t)
(require 'cider)
(require 'subr-x)
(defvar ed-port (string-to-number (getenv "NREPL_PORT")))
(defvar ed-dir (file-name-as-directory (getenv "WORK_DIR")))
(defvar ed-client "cider")
(defvar ed-msgs nil)
(advice-add 'message :before (lambda (fmt &rest a) (when fmt (push (apply #'format fmt a) ed-msgs))))

(defun ed-res (step status &optional detail)
  (princ (format "RESULT %s %s %s %s\n" ed-client step status
                 (replace-regexp-in-string "\n" "|" (or detail "")))))
(defun ed-has (step text needle)
  (if (and text (string-match-p (regexp-quote needle) text)) (ed-res step "PASS")
    (ed-res step "FAIL" (format "expected [%s] got [%S]" needle text))))
(defun ed-dict (d &rest ks) (mapcar (lambda (k) (nrepl-dict-get d k)) ks))
(defun ed-ev (code)
  (with-current-buffer (cider-current-repl) (ed-ev1 code)))
(defun ed-ev1 (code)
  "Sync eval. Return (value out err status ex)."
  (let ((r (cider-nrepl-sync-request:eval code)))
    (list (nrepl-dict-get r "value") (nrepl-dict-get r "out") (nrepl-dict-get r "err")
          (nrepl-dict-get r "status") (nrepl-dict-get r "ex"))))
(defmacro ed-try (step &rest body)
  `(condition-case e (progn ,@body) (error (ed-res ,step "FAIL" (format "lisp error: %S" e)))))


(defun ed-wait (pred secs)
  (let ((n 0)) (while (and (< n (* 5 secs)) (not (funcall pred))) (setq n (1+ n)) (accept-process-output nil 0.2)) (funcall pred)))
(defun ed-join (x) (if (listp x) (mapconcat #'identity x ",") (format "%s" x)))

;; ---- connect
(ed-try "connect"
  (cider-connect-clj (list :host "127.0.0.1" :port ed-port :project-dir ed-dir))
  (if (ed-wait (lambda () (and (cider-connected-p) (cider-current-repl nil nil))) 20)
      (progn (ed-wait (lambda () nil) 3) (ed-res "connect" "PASS"))
    (ed-res "connect" "FAIL" "no REPL connection after 20s")))

(defvar ed-repl (cider-current-repl))
(unless ed-repl (princ "FATAL no repl\n") (kill-emacs 1))

;; ---- eval
(ed-try "eval" (let ((r (ed-ev "(+ 1 2)"))) (ed-has "eval" (car r) "3")))
(ed-try "print" (let ((r (ed-ev "(println \"hello-out\")"))) (ed-has "print" (nth 1 r) "hello-out")))
(ed-try "error" (let ((r (ed-ev "(/ 1 0)")))
  (ed-has "error" (nth 2 r) "Divide by zero")
  (princ (format "INFO error-reply status=%S ex=%S err=%S\n" (nth 3 r) (nth 4 r) (nth 2 r)))))
(ed-try "multi" (let ((r (ed-ev "(def a 1) (def b 2) (+ a b)"))) (ed-has "multi" (car r) "3")))

;; ---- load file
(ed-try "load"
  (let ((f (expand-file-name "load.clj" ed-dir)))
    (with-temp-file f (insert "(ns user)\n(defn sq [x]\n  (* x x))\n(println \"loaded\")\n"))
    (with-current-buffer ed-repl
      (find-file-noselect f)
      (cider-load-file f))
    (ed-wait (lambda () nil) 2)
    (let ((r (ed-ev "(sq 9)"))) (ed-has "load" (car r) "81"))))

;; ---- complete
(ed-try "complete"
  (let ((c (with-current-buffer ed-repl (cider-complete "ma"))))
    (princ (format "INFO complete(%d) sample=%S\n" (length c) (seq-take c 5)))
    (if (and c (seq-some (lambda (x) (string-match-p "map" x)) c)) (ed-res "complete" "PASS")
      (ed-res "complete" "FAIL" (format "%S" c)))))

;; ---- doc
(ed-try "doc"
  (let ((i (with-current-buffer ed-repl (cider-var-info "map" t))))
    (princ (format "INFO var-info=%S\n" i))
    (if (and i (string-match-p "Returns a lazy" (format "%S" i))) (ed-res "doc" "PASS")
      (ed-res "doc" "FAIL" (format "%S" i)))))

;; ---- interrupt
(ed-try "interrupt"
  (let ((resps nil))
    (with-current-buffer ed-repl
      (cider-nrepl-request:eval "(Thread/sleep 60000)" (lambda (r) (push r resps)))
      (ed-wait (lambda () nil) 1)
      (cider-interrupt)
      (ed-wait (lambda () (seq-some (lambda (r) (member "done" (nrepl-dict-get r "status"))) resps)) 8))
    (let* ((st (apply #'append (mapcar (lambda (r) (nrepl-dict-get r "status")) resps)))
           (r2 (ed-ev "(+ 20 22)")))
      (if (and (member "interrupted" st) (equal (car r2) "42")) (ed-res "interrupt" "PASS" (format "status=%S" st))
        (ed-res "interrupt" "FAIL" (format "status=%S next=%S" st r2))))))

;; ---- stdin
(ed-try "stdin"
  (let ((resps nil) (answered nil))
    (with-current-buffer ed-repl
      (cider-nrepl-request:eval "(read-line)"
        (lambda (r) (push r resps)
          (when (member "need-input" (nrepl-dict-get r "status"))
            (setq answered t)
            (nrepl-request:stdin "typed-text\n" (lambda (_) nil) ed-repl))))
      (ed-wait (lambda () (seq-some (lambda (r) (member "done" (nrepl-dict-get r "status"))) resps)) 8))
    (let ((vals (delq nil (mapcar (lambda (r) (nrepl-dict-get r "value")) resps))))
      (if (and answered (member "\"typed-text\"" vals)) (ed-res "stdin" "PASS")
        (ed-res "stdin" "FAIL" (format "need-input=%S resps=%S" answered (reverse resps)))))))

;; ---- close
(ed-try "close"
  (progn
    (with-current-buffer ed-repl (princ (format "REPLBUF: %S\n" (buffer-substring-no-properties (point-min) (point-max)))))
    (cl-letf (((symbol-function 'y-or-n-p) (lambda (&rest _) t)) ((symbol-function 'yes-or-no-p) (lambda (&rest _) t)))
      (cider-quit))
    (ed-wait (lambda () nil) 1)
    (if (not (cider-connected-p)) (ed-res "close" "PASS") (ed-res "close" "FAIL" "still connected"))))
(princ (format "MSGS: %S\n" (reverse ed-msgs)))
