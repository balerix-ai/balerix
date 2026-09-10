Balerix is a coding agent control plane and orchestrator that can he programmatically controlled via HTTP APIs.

in the first iteration we want to perform orchestration within the same box / system, but keep in mind in the future there will be a separation between balerix operator (control plane) and balerix agent (data plane) and it will a Kubernetes-native operator.

global principles:
- Use XDG directories when possible for storing any state
- Implement in Rust and use mise (tool management) and nono (sandboxing) as libraries as much as possible.

balerix has the following commands:
- serve - starts the balerix daemon process that acts as a control plane and can service HTTP requests, should support a `-d` / `--deatch` flag to start the daemon in the background.
- client commands:
  - up - create a fleet of agents according to the supplied yaml config and name of the fleet and reconcile the actual orchestration state to the config and wait until it matches before exiting. errors if the fleet with the given 
  - update - update a fleet of agents according to the supplied yaml config and name of the fleet. it's the update version of "up" where up creates a completely new fleet.
  - down - tear down a fleet, include some options to preserve fleet state that can be re-used on the next `up` invocation on the same fleet name.

all these client commands are thin wrappers around the server HTTP calls and also by default inspect the system for ~/.claude settings to use as defaults for all the agents (including login credentials/ etc) it should encrypt them for the server call (https server by default)

fleet configuration should be yaml and handle merging values across fleet settings, crew settings, and agent settings cleanly.

a balerix fleet is a tmux session consisting of claude code (in the future other agents as well) instances on every screen. fleets have a notion of "crews" as well. All the agents in a crew share the particular configuration and you can specify which agents belong to the crew. the configuration should be fully declarative - you should be able to specify claude settings, what text to send to claude in what sequence as well as how to respond to particular hooks. for now support a state machine like setup where you can do regexp matching on claude code hook event fields and match on particular claude code events and specify action (send text / turn) and/or switch state to something else, use your best judgement and how to make this configuration intuitive.

for the state machine runtime flow, there should be full prometheus metrics exported so monitoring has first-class support.

if it makes it easy to specify the flow and event respones in a python-like type-safe DSL design, but defer it as far as possible until use caes demand it. 

each balerix fleet agent can configure declaratively claude settings, sandbox settings (from nono. yaml-ize the config), mise settings (for management of system tools that can be available to the agent) and agent runner-specific settings. an agent runner can either be tmux window / pane, docker container, kubernetes pod, so keep that level abstract - we'll priortize tmux session per agent crew, and tmux windows per agent itself. Note that `tmux` should be referenced as a system tool in the system mise config for balerix.

each agent should be fully isolated and the environment for the agent run should be constraining $HOME, $CLAUDE_* directory variables, etc and other standard variables should be inside of balerix's state directory and nono sandbox permissions should restrict access to these locations.

Every balerix agent has a git repo associated with it (with first class support got GitHub repos) and the agent will make that repo its workspace (the cwd it attaches to in tmux). configuration should allow the agent to configure git permissions via the `gh` tool (which should also be included in `balerix`'s system tools mise config that all agents inherit).
