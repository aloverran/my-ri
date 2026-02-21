# SolidJS Guidelines

SolidJS is powerful because it has fine-grained reactivity. The idea is that we keep the minimal stored state, even sometimes having no state at all, and the UI is powered by a well defined graph of computation functions. This is powerful because it guarantees that UI will always stay up to date in the precense of many different ways underlying state can change (network calls, user input, async functions, etc), and it means we can remove most state *entirely* from the application! In SolidJS state is smell, we try to keep it as small and total as possible, and as precisely typed as possible. 

This follows from the functional / rust-like philosophy. Build the application so that if it compiles it is correct.

Similarly, we try to avoid using non-reactive features like mount, cleanup, etc except when we have to interact with non-reactive libraries or DOM features (ie. d3js). One wayward line and we'll create a 1% bug that appears rarely in only various state setups.

Where we must, we would prefer to wrap non-reactive modules into reactive ones, rather than splaying out the lifecycle code mixed into the reactive code. We build these components so we can draw boxes around them and forget about them!

# Typescript Guidelines

We are very thoughtful with how we use types. Typescript is very hard to write in a total way because so many values can easily have values outside the domain we intend to model. Strings can have '' or null, etc. Numbers can have NaN. Objects can have strange accessors, etc. Where possible we try to keep everything deeply and precisely modeled, even where that makes types harder to read. We define string enums, leverage advanced typescript typing features. We are a smart functional programmer and are not afraid to be precise where it will model the domain better.

For us, frontend isn't cheap, throwaway code. I am a senior engineer and I want it to be robust. We don't write it to be easily usable by random junior web-devs. We write it smart, and clear, and with an eye to the overall architecture of the codebase.

JSX can get hard to read with lots of HTML, so we use comments deliberately to make it easier to skim. We also like to comment chunkier components to explain why they exist and how they engage with the rest of the system.