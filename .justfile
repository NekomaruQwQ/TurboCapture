set shell := ["nu", "-c"]

# == Recipes for development experience ==
# List all recipes.
list:
    just --list
# Compile the HLSL entries declared in shaders.toml with fxc.
compile-shaders:
    use . *; compile-shaders
# Run the specified component.
run name *args: compile-shaders
    use . *; run-{{name}} {{args}}
# Run the specified `bun` command in the frontend directory.
bun *args:
    cd frontend; bun {{args}}
# Run the specified `tsc` command in the frontend directory.
tsc *args:
    cd frontend; bunx --bun tsc --noEmit {{args}}
# Run the specified `svelte-check` command in the frontend directory.
svc *args:
    cd frontend; bunx --bun svelte-check --tsconfig tsconfig.json {{args}}

# == Recipes for JJ version control ==
# Move the specified bookmark to the specified revision and push all changes to GitHub.
push bookmark="dev" revision="@-":
    jj bookmark move {{bookmark}} --to={{revision}}
    jj git push --all
# Pull the latest changes from GitHub and reset the working copy to the main branch.
pull bookmark="dev":
    jj git fetch
    jj new -r {{bookmark}}@origin

# == Recipes for server RESTful APIs ==
# Make an HTTP GET request
get path *args:
    use . *; http get (get-url "{{path}}") {{args}}
# Make an HTTP PUT request with the specified data.
put path data *args:
    use . *; http put (get-url "{{path}}") "{{data}}" {{args}}
# Make an HTTP POST request with the specified data.
post path data *args:
    use . *; http post (get-url "{{path}}") "{{data}}" {{args}}
# Trigger the server to refresh its configuration.
refresh:
    just post "/api/refresh" ""
get-string:
    just get "/api/strings"
set-string key value:
    just put "/api/strings/{{key}}" "{{value}}"
