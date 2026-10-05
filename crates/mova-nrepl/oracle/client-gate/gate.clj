;; Client gate: the JVM nREPL 1.8.0 client-side tests, run against `mova nrepl`.
;;
;;   ./run.sh [--only SUBSTR ...] [--list]
;;
;; How: the tests start their server with `nrepl.server/start-server` in the
;; same JVM. This script replaces that function: it starts `mova nrepl` as a
;; child process (same flags the real options would give) and returns a stand-in
;; that looks like the Server record (`:port :host :socket`, `.close`). Then it
;; runs the test vars of `nrepl.core-test` and the server-facing ones of
;; `nrepl.cmdline-test`, each with a timeout, and prints one line per case.
;;
;; Cases that start a JVM child process (`java ... nrepl.main`) or need a JVM
;; server for the other side (`ack`) are re-written below as `gate/...` cases that use
;; the Mova binary. Cases that test JVM-only things (class loaders, server
;; internals, JVM client helper functions) are listed in `skipped` with the reason.
(ns gate
  (:require [clojure.java.io :as io]
            [clojure.string :as str]
            [clojure.test :as t]
            [nrepl.cmdline]
            [nrepl.core :as nrepl]
            [nrepl.socket]
            [nrepl.version]
            [nrepl.ack :as ack]
            [nrepl.server :as server]
            [nrepl.transport :as transport]
            [nrepl.test-helpers :refer [eval-value1 with-timeout]])
  (:import (java.io Closeable)
           (java.lang ProcessBuilder$Redirect)))

(def mova-bin (or (System/getenv "MOVA")
                  ;; default: <repo>/target/release/mova (this file is <repo>/crates/mova-nrepl/oracle/client-gate/gate.clj)
                  (str (-> (java.io.File. (str *file*)) .getAbsoluteFile .getParentFile .getParentFile .getParentFile .getParentFile .getParentFile)
                       "/target/release/mova")))
(def mova-tls-bin (System/getenv "MOVA_TLS"))
(def orig-start-server server/start-server)

(defn- tmpdir [] (str (java.nio.file.Files/createTempDirectory "mova-gate-" (into-array java.nio.file.attribute.FileAttribute []))))

(defn spawn
  "Starts `mova nrepl args...` in a fresh directory. Returns {:proc :banner :lines :dir}."
  [bin args]
  (let [dir (tmpdir)
        pb (doto (ProcessBuilder. ^java.util.List (into [bin "nrepl"] args))
             (.directory (io/file dir))
             (.redirectError ProcessBuilder$Redirect/INHERIT))
        proc (.start pb)
        rdr (io/reader (.getInputStream proc))
        banner (.readLine rdr)
        lines (atom [])]
    (future (try (loop [] (when-let [l (.readLine rdr)] (swap! lines conj l) (recur))) (catch Exception _)))
    {:proc proc :banner banner :lines lines :dir dir}))

(defn mova-start-server
  [& {:keys [port bind socket transport-fn tls? tls-keys-str tls-keys-file ack-port] :as opts}]
  (let [args (cond-> []
               port (into ["--port" (str port)])
               bind (into ["--bind" bind])
               socket (into ["--socket" (.getAbsolutePath (io/file socket))])
               transport-fn (into ["--transport" (str (symbol transport-fn))])
               tls-keys-str (into ["--tls-keys-str" tls-keys-str])
               tls-keys-file (into ["--tls-keys-file" tls-keys-file])
               ack-port (into ["--ack" (str ack-port)]))
        {:keys [proc banner]} (spawn (if (or tls? tls-keys-str tls-keys-file) (or mova-tls-bin mova-bin) mova-bin) args)
        [_ p h] (re-find #"on port (\d+) on host (\S+)" (str banner))]
    (when-not banner (throw (ex-info "mova nrepl printed no banner" {:args args})))
    ;; a real `nrepl.server.Server` record (the tests type-hint it); its server-socket kills the child
    (server/map->Server {:server-socket (reify Closeable (close [_] (.destroy proc) (.waitFor proc)))
                         :host (when-not socket h)
                         :port (some-> p parse-long)
                         :socket socket
                         :open-transports (atom #{})
                         :transport transport-fn})))

;; ---------------------------------------------------------------------------
;; running one test var with a timeout, capturing the reports
;; ---------------------------------------------------------------------------

(def ^:private timeout-ms (parse-long (or (System/getenv "GATE_TIMEOUT_MS") "90000")))

(defn run-var [v]
  (let [fails (atom []) n (atom 0)]
    (let [f (future
              (binding [*print-length* nil *print-level* nil
                        t/report (fn [m]
                                   (case (:type m)
                                     :pass (swap! n inc)
                                     :fail (swap! fails conj (str "FAIL " (pr-str (:message m)) " expected " (pr-str (:expected m)) " actual " (pr-str (:actual m))))
                                     :error (swap! fails conj (str "ERROR " (let [a (:actual m)] (if (instance? Throwable a) (str (class a) ": " (ex-message a)) (pr-str a)))))
                                     nil))]
                (t/test-vars [v])))
          r (deref f timeout-ms ::timeout)]
      (when (= r ::timeout) (future-cancel f) (swap! fails conj (str "TIMEOUT after " timeout-ms " ms")))
      {:pass @n :fails @fails})))

;; ---------------------------------------------------------------------------
;; cases that spawn JVM processes upstream: re-written for Mova
;; ---------------------------------------------------------------------------

(defn- transport-names [] ["nrepl.transport/bencode" "nrepl.transport/edn"])

(defn case-ack []
  ;; cmdline_test/ack + explicit-port-argument, and core_test/test-ack the other way round
  (doseq [tn (transport-names)
          :let [tf (requiring-resolve (symbol tn))]]
    (with-open [^Closeable receiver (orig-start-server :transport-fn tf :handler (ack/handle-ack (server/default-handler)))]
      (ack/reset-ack-port!)
      (let [bind-port (with-open [s (java.net.ServerSocket. 0)] (.getLocalPort s))
            p (spawn mova-bin ["--port" (str bind-port) "--ack" (str (:port receiver)) "--transport" tn])]
        (try
          (let [acked (ack/wait-for-ack 20000)]
            (assert (= bind-port acked) (str tn ": acked " acked ", expected " bind-port))
            (with-open [^Closeable tr (nrepl/connect :port acked :transport-fn tf)]
              (assert (= 2 (eval-value1 (nrepl/client tr 5000) '(+ 1 1))))))
          (finally (.destroy ^Process (:proc p))))))
    ;; random port, no --port
    (with-open [^Closeable receiver (orig-start-server :transport-fn tf :handler (ack/handle-ack (server/default-handler)))]
      (ack/reset-ack-port!)
      (let [p (spawn mova-bin ["--ack" (str (:port receiver)) "--transport" tn])]
        (try
          (let [acked (ack/wait-for-ack 20000)
                [_ port] (re-find #"on port (\d+)" (:banner p))]
            (assert (= (parse-long port) acked)))
          (finally (.destroy ^Process (:proc p))))))))

(defn case-fs-socket []
  ;; cmdline_test/basic-fs-socket-behavior: `(System/exit 42)` over the socket stops the server with status 42
  (let [dir (tmpdir) sock (str dir "/socket")
        p (spawn mova-bin ["-s" sock])]
    (try
      (with-open [s (nrepl.socket/unix-client-socket sock)]
        (let [out (nrepl.socket/buffered-output s)]
          (#'transport/safe-write-bencode out {:code "(System/exit 42)" :op :eval})
          (.flush ^java.io.Flushable out)))
      (assert (.waitFor ^Process (:proc p) 20 java.util.concurrent.TimeUnit/SECONDS) "server did not exit")
      (assert (= 42 (.exitValue ^Process (:proc p))) (str "exit status " (.exitValue ^Process (:proc p))))
      (finally (.destroy ^Process (:proc p))))))

(defn case-tty []
  ;; cmdline_tty_test/tty-server, with `mova nrepl` and no JVM property
  (let [p (spawn mova-bin ["--transport" "nrepl.transport/tty"])
        [_ port] (re-find #"on port (\d+)" (:banner p))
        c (org.apache.commons.net.telnet.TelnetClient.)]
    (try
      (.connect c "localhost" (int (parse-long port)))
      (.setSoTimeout c 20000)
      (with-open [out (java.io.PrintStream. (.getOutputStream c))
                  br (io/reader (.getInputStream c))]
        (doseq [l ["(+ 1 2)" "#?(:clj :clj-form)" "#?(:cljs :cljs-form)"
                   "(clojure.core/require '[clojure.set :as sets])" "::sets/xyz"
                   "(clojure.core/require '[clojure.string :as str])" "{::sets/x 1 ::str/x 2}"]]
          (.println out l))
        (.flush out)
        (let [got (vec (repeatedly 8 #(.readLine ^java.io.BufferedReader br)))
              want [#"^;; nREPL" #"^;; Clojure" "user=> 3" "user=> :clj-form" "user=> nil" "user=> :clojure.set/xyz" "user=> nil" "user=> {:clojure.set/x 1, :clojure.string/x 2}"]]
          (doseq [[g w] (map vector got want)]
            (assert (if (string? w) (= g w) (re-find w g)) (str "got " (pr-str got))))))
      (finally (.disconnect c) (.destroy ^Process (:proc p))))))

(defn case-help []
  ;; `--help` is `(println (help))`
  (let [pb (ProcessBuilder. ^java.util.List [mova-bin "nrepl" "--help"])
        out (slurp (.getInputStream (.start pb)))
        want (str (#'nrepl.cmdline/help) "\n")]
    (assert (= want out) "help text differs")))

(defn case-version []
  (let [out (slurp (.getInputStream (.start (ProcessBuilder. ^java.util.List [mova-bin "nrepl" "-v"]))))]
    (assert (= (str (:version-string nrepl.version/version) "\n") out) out)))

(defn case-banner-format []
  ;; cmdline_test/server-started-message, but against the real banner of every transport and bind form
  (doseq [[args pat] [[[] #"nREPL server started on port \d+ on host 127\.0\.0\.1 - nrepl://127\.0\.0\.1:\d+"]
                      [["-b" "localhost"] #"nREPL server started on port \d+ on host localhost - nrepl://localhost:\d+"]
                      [["-t" "nrepl.transport/edn"] #".* - nrepl\+edn://127\.0\.0\.1:\d+"]
                      [["-t" "nrepl.transport/tty"] #".* - telnet://127\.0\.0\.1:\d+"]]]
    (let [p (spawn mova-bin args)]
      (try (assert (re-matches pat (:banner p)) (:banner p))
           (finally (.destroy ^Process (:proc p)))))))

(defn case-tls-url []
  ;; cmdline_test/can-connect-via-tls-url, against `mova nrepl --tls-keys-str` (needs a build with --features tls)
  (assert mova-tls-bin "SKIP: set MOVA_TLS to a mova built with --features tls")
  (let [{:keys [ca-cert server-cert server-key client-cert client-key]} ((requiring-resolve 'com.github.ivarref.locksmith/gen-certs) {:duration-days 1})
        server-keys (str ca-cert server-cert server-key)
        client-keys (str ca-cert client-cert client-key)]
    (doseq [tn (transport-names)]
      (let [p (spawn mova-tls-bin ["--tls-keys-str" server-keys "--transport" tn])
            url (second (re-find #" - (\S+)$" (:banner p)))
            results (atom [])
            >devnull (fn [& _] nil)]
        (try
          (binding [*in* (java.io.PushbackReader. (java.io.StringReader. "(+ 1 2)"))]
            (with-redefs [nrepl.cmdline/clean-up-and-exit >devnull]
              (with-timeout 60000
                (#'nrepl.cmdline/run-repl url nil {:tls-keys-str client-keys :prompt >devnull :err >devnull :out >devnull
                                                   :value #(swap! results conj %)}))
              (assert (= ["3"] @results) (str url " " @results))))
          (finally (.destroy ^Process (:proc p))))))))

(def gate-cases
  {"gate/ack" case-ack
   "gate/fs-socket-exit-42" case-fs-socket
   "gate/tty-server" case-tty
   "gate/help-text" case-help
   "gate/version" case-version
   "gate/banner-format" case-banner-format
   "gate/tls-url" case-tls-url})

(defn run-fn [name f]
  (let [fut (future (binding [*print-length* nil *print-level* nil] (try (f) {:pass 1 :fails []}
                         (catch Throwable e {:pass 0 :fails [(let [m (or (ex-message e) (str e))]
                                                               (if (str/starts-with? m "SKIP") m (str "ERROR " m)))]}))))
        r (deref fut timeout-ms ::timeout)]
    (if (= r ::timeout) (do (future-cancel fut) {:pass 0 :fails ["TIMEOUT"]}) r)))

;; ---------------------------------------------------------------------------
;; which upstream vars run
;; ---------------------------------------------------------------------------

(def skipped
  {"nrepl.core-test/version-sanity-check"                 "JVM: checks the Clojure version of the test JVM"
   "nrepl.core-test/ensure-server-closeable"              "JVM: closes the in-process Server record"
   "nrepl.core-test/non-interruptible-stop-thread"        "JVM: Thread.stop on a Java loop"
   "nrepl.core-test/hotloading-common-classloader-test"   "JVM: class loaders"
   "nrepl.core-test/classloader-chain-doesnt-grow-test"   "JVM: class loaders"
   "nrepl.core-test/custom-context-classloader-is-not-overwritten" "JVM: class loaders"
   "nrepl.core-test/custom-context-classloader-is-used-for-loading" "JVM: class loaders"
   "nrepl.core-test/test-ack"                             "needs a JVM server with the ack middleware as the receiver; see gate/ack"
   "nrepl.cmdline-test/ack"                               "spawns java nrepl.main; see gate/ack"
   "nrepl.cmdline-test/explicit-port-argument"            "spawns java nrepl.main; see gate/ack"
   "nrepl.cmdline-test/basic-fs-socket-behavior"          "spawns java nrepl.main; see gate/fs-socket-exit-42"
   "nrepl.cmdline-test/can-connect-via-tls-url"           "uses the JVM Server record for the URL; see gate/tls-url"
   "nrepl.cmdline-test/repl-intro"                        "JVM client text (java.vm.name)"
   "nrepl.cmdline-test/help"                              "tests the JVM help fn; see gate/help-text"
   "nrepl.cmdline-test/parse-cli-values"                  "JVM-only function"
   "nrepl.cmdline-test/args->cli-options"                 "JVM-only function"
   "nrepl.cmdline-test/connection-opts"                   "JVM-only function"
   "nrepl.cmdline-test/server-opts"                       "JVM-only function"
   "nrepl.cmdline-test/server-started-message"            "JVM-only function; see gate/banner-format"
   "nrepl.cmdline-test/read-form-for-server-passthrough"  "JVM client reader function"
   "nrepl.cmdline-test/connect-url-scheme-test"           "JVM client function"
   "nrepl.cmdline-test/ensure-url-scheme-support-test"    "JVM client function"
   "nrepl.cmdline-test/unix-socket-url-test"              "JVM client function"
   "nrepl.cmdline-test/url-conflicting-options-test"      "JVM client function"
   "nrepl.cmdline-test/tls-url-option-validation"         "JVM client function"
   "nrepl.cmdline-test/cmdline-namespace-resolution"      "see note: runs (kept), not skipped"})

(defn upstream-vars []
  (require 'nrepl.core-test 'nrepl.cmdline-test)
  (for [ns-sym '[nrepl.core-test nrepl.cmdline-test]
        v (sort-by (comp :line meta) (vals (ns-interns ns-sym)))
        :when (:test (meta v))]
    v))

(defn -main [& args]
  (let [only (->> args (remove #{"--list"}) (remove #(= % "--only")))
        list? (some #{"--list"} args)
        pass? (fn [name] (or (empty? only) (some #(str/includes? name %) only)))]
    (alter-var-root #'server/start-server (constantly mova-start-server))
    ;; the tests' own `nrepl.server/start-server` calls now start Mova
    (let [results
          (concat
           (for [v (upstream-vars)
                 :let [name (str (ns-name (:ns (meta v))) "/" (:name (meta v)))]
                 :when (pass? name)]
             (if-let [why (and (not= name "nrepl.cmdline-test/cmdline-namespace-resolution") (skipped name))]
               [name :skip why]
               (if list? [name :list ""]
                   (let [{:keys [pass fails]} (run-var v)]
                     [name (if (seq fails) :fail :pass) (str "asserts " pass (when (seq fails) (str "; " (str/join " | " (take 3 fails)))))]))))
           (for [[name f] gate-cases :when (pass? name)]
             (if list? [name :list ""]
                 (let [{:keys [fails]} (run-fn name f)]
                   (cond (and (seq fails) (str/starts-with? (first fails) "SKIP")) [name :skip (first fails)]
                         (seq fails) [name :fail (str/join " | " fails)]
                         :else [name :pass ""])))))]
      (doseq [[name st why] results]
        (println (format "%-6s %-62s %s" (str/upper-case (clojure.core/name st)) name (let [w (str why)] (if (> (count w) 2200) (subs w 0 2200) w)))))
      (let [c (frequencies (map second results))]
        (println (format "\npass %d  fail %d  skip %d" (c :pass 0) (c :fail 0) (c :skip 0))))
      (shutdown-agents)
      (System/exit (if (some #{:fail} (map second results)) 1 0)))))
