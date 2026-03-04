A list of todos and wishlist features for ri.

- List all files in cwd based on the .gitignore and prep a small prompt that is injected as the first user message in the workspace. Find ways to compress the file paths and other info while still providing 'broad vision'.
- Support compaction of some kind.
- Support queued messages (either injected even after a single llm turn or once the full agent loop completes).
- Fork button to create new session off of a message.
- Session parenting! Hide sub-sessions in the main view, and create a navigator in the chat view.
  - A deep message view that shows the session (and children) history pointer and the graph of messages reachable from this session.

# new tools

I think we want:

- createMessage(body: String, Role) -> MessageId
- createContext([MessageId], parents: [ContextId]) -> ContextId
 
Then do we need something to manipulate the sessions? Sessions feel like they are user facing. But maybe that's because we need something to represent a 'running agent' or.. a locked line of contexts..? Contexts are kinda like patches in pijul the way they branch out without needing parallel locking. Hm.

But we need some way to get the context id of the final context after the agent finishes? Either a promise key.. or.. something? Either it's a pointer that slowly updates. Or it's synchronous and just holds that in memory. I suppose that's the same as 'background task' ids? I guess that's why we can read the session pointers? But we assume they are random rather than persistant.

# parallel tool calls

Agent responses should be able to request tool calls to be run in parallel. Possibly this should be default, rather than sequential. Or perhaps it should be per-tool (ie. a web search can always be parallel because it is stateless)?

In general I have found that the agent is pretty terrible about judging whether tool calls should be in the background or not. 


# ability to select type of message to manually inject

I want to be able to inject a message as if the model said it. I wonder how that might affect its reasoning ability.

# update session id.. in system prompt when runagent and old session id embedded in prompts?

although if we fork or bind a new session id........ hm. that blows the cache! damn. we could just add it as a tool or user or model message or something? like system reminder probably.

# we have no system prompts for sub-agents right now!

# fix session id generation

runAgent will generate a session_id even if one is passed, leading to invalid session reads later.

# add the ability to mark a message that has been cached! 

it'd be nice to be able to reason about potential cache usage from the message graph. we can know this based on which message have been submitted as context to a model? but there's something funky here about the messages provenance being mixed with history. the message itself isn't cached -- it's the message + provenance. maybe that's just a job for a rust function though and we can build that out of submission flags. wait I guess all messages with provenance have been cached lol... nvm? oh and caching is by provider. all in all this feels more like a view function not stored state.

# add message_id prepended to all context messages

if we prepend <message_id>big_cat_28</message_id> to each message block, then will it mean the agent will be smart enough to use them when constructing runAgent calls? Could we finally say "compact your history?". note: can we do that now with readsession lol.

we'll need to include instructions in the system prompt.

# emit suredness of answer

After every message i want it to emit how confident it is in the answer. I'm not sure what the right format is though.. percent? short phrase? emit a short meta-statement on confidence? etc. i want to use this to inject additional thinking from smart models automatically.. basically to solve the issue where I odnt feel the model is very good about figuring out *when* it should invoke the big models. sometimes you discover the necessity down the path. so maybe it's more like 'if you get stuck in a loop on something' or.. 'if something needs verification'. it's a trick though because the models hallucinate loosely still various apis. so maybe it's better to treat this as grounding? the smart model kicks off and reviews the session entirely? that's kind of weird! or maybe a fast model looks through and determines what to send to the big model? 

how do humans do it? we do this with learned intuition -- a set of vague guides of when what we think we know might be wrong. but honestly this is dunning kruger in full effect. what solves that? maybe a short philosophy on the wisdom of knowledge? I want to run a review model on a session and see if I can output which messages feel sketchy or need verification.

lets consider an example: tailwind v4 was released last month. we know that our training window doesn't include much about it. but how can we *know* that without some kind of grounding? maybe we use a web search for concrete topics aimied at collecting info on the inherent topics? like haiku->gemini web grounding->inject as model. Could kind of slowly guide the model as it works.

# flowcharts in the system prompt

The model conforms pretty well to the flowcharts I give but I wonder if it would do better when the flowchart is put in the system prompt? 

# start session with initial query

rather than name form the start. this would also allow us to customize the system prompt based on the prompt template! that'd be cool! and kinda neat. you could update the flowchart system prompt with a command if you don't mind blowing hte cache.

use a cheap model like haiku to name the session when enough confidence is gained. (like claude does)

# button to continue

i want to see if just forcing a new completion rather than a user message with 'continue' acts any different.

# Extra thinking

we could very easily get 'extra thinking' on something by forcing more completion or by using a sub-prompt asking it to design/think/architect for so many tokens, etc.

# inject token counts and usage into actual prompt

we want to do this again so we can easily add things like 'run a sub agent until 100k tokens designing this and read the result.'

this is particularly useful for research where we get radically more thorough details when forcing full context usage for exploration.

# build back a research mode that we had! 

this worked well -- let's keep using it!

# Add gemini api as provider

we will add the gemini api itself (interactions rest api) as a provider and use a free tier api key. and maybe a special key to get our $100/mo credit if we can. (may need to go through vertex for this i guess?)

however, big gemini pro is suuuuper expensive so we only want to use it in runMessage form. gemini 3 flash has a liberal free tier so we can probably get it to do agent stuff. 

so we need a way to specify that certain models should be blocked for runAgent? feels like this should be in our runAgent tool, probably.


# fork 

i want to be able to run several completions and see how they differ. different models, etc. could be achieved with jumping, but i want a better interface.

# visual graph viewer 

There's a lot of data so we want to show it in a visualization style that feels a bit like a minimal map with some nice flourishes to help with data viz. Messages should be shown as little rectangular filled boxes. Hovering a message will show lines to the set of other messages that make up its provenance. Boxes are colored by the message color already existing. Messages should form a directed graph flowing from top to bottom, with most recent appended messages at the bottom. 

Sessions are filled in circles that point at the latest message in the session reflog. Hovering a session renders numbers in each of the message boxes 1..N showing the history of the session reflog and what it pointed to.
 
# run message
Implement a tool: `runMessage`. This tool invokes an LLM provider for a single response. It does not provide tools or functions for calling. When complete the message_id is returned.

?: should it be async? we want the ability to fan out! but async requires two tool calls and requires the model to remember to check in. I guess what we really want is the ability to fan out tool calls in general? Basically mark when the tools can be executed in parallel. But will be returned before the next turn.

Honestly only runMessage really needs to be parallel. All the others are so fast that there's really no point.

Ok yeah so never 'async' but parallel ok (or even task graph?). We've found that anything that executes in the background needs an explicit cue to check in and wait, otherwise it gets forgotten about. The other option is to have the response be injected as a user message. Or perhaps queue up in an inbox that the agent can see and open? Interesting to consider that rather than forcing the mail into their context. Hm. Primarily I'm not sure the inbox is better than a injection. Either way it tends to happen too late and get the model confused. I think in part because it think it's a user message and that comes with some certain types of training! We'd probably have to inject it as if the model itself output it? Now *that's* weird. Or maybe a tool result. In general most of these agents don't see several model messages in a row, so I wonder how they would treat it.

Maybe we want to inject a system reminder before the generation when there are outstanding bg processes? and their state? so like {subagent name state}

and hopefully the agent response would (instead of sumarizing) choose to wait? i feel like that would help forgetting without get 'pinged' after a summary -- need to make it choose to wait or explicitly ignore. explicitly ignore is actually fine? it's a judgement call im ok to leave, means we need to adjust the judgement for it, as long as it didn't literally forget. maybe inject as a tool call, yeah.

# discussion

it's weird that the signal for the agent to stop is that it doesn't issue any tool calls. actually is that true? yeah seems so. like, we could just kick it off again. but i guess my question is what that would look lik.

We currently have no concept of 'agent is running'. as we break away from traditional assistant-like user/asst flows, there's no.. obvious way to know if something is still working? A few options:
 - end sigil. the summary acts as this often, or we could force one manually
 - subjective. direct when reading sessions to guess if the agent is still working
I guess though it's working if there are outstanding tool calls or a result is streaming in? or if the last thing we appended was a tool call? yeah i suppose it's only not working on non tool asst call. or rather it's working when there is a tool call outstaning or an asst that returns tool call

## asking for help

thinking on how and when humans learn to ask for help. in many ways I'll do a quick google search just to cue off to see how much my knowledge is wrong or if there's little signals i'm missing.
maybe the right thing to do is to test the water. you do a smoke test google search or flash geini query to see how much it surprises you. this then informs whether you should ask for help further.

# code quality:

The  runAgent tool should just use the store for sessions field in the app state the way readsession does. don't spin up a new store

Refactor (iteraions 3): The server should not keep state except for running an agent loop. if there's no agent loop running, killing the server and reloading should have no effect. This means everything should be persisting to disk! We're a db with a light layer of control, not a live app server. Find a portion of the code that violates this principle and refactor the codebase to fix it. 