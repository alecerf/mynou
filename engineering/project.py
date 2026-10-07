"""Optional native Project adapter. Null configuration means no API calls."""
from control import labels


def sync(api, issue_number):
    project = api.cfg.get("project")
    if project is None:
        return {"configured": False, "backlog": "Native Issues and structured labels"}
    issue = api.rest("GET", f"issues/{issue_number}")
    result = api.graphql("""mutation($project:ID!,$issue:ID!) {
      addProjectV2ItemById(input:{projectId:$project,contentId:$issue}) { item { id } }
    }""", {"project": project["id"], "issue": issue["node_id"]})
    item = result["addProjectV2ItemById"]["item"]["id"]
    for family, field in project["fields"].items():
        matches = [n.split(":", 1)[1] for n in labels(issue) if n.startswith(family + ":")]
        if len(matches) != 1 or matches[0] not in field["options"]:
            raise ValueError("Project sync requires one mapped native Issue label per field")
        api.graphql("""mutation($project:ID!,$item:ID!,$field:ID!,$option:String!) {
          updateProjectV2ItemFieldValue(input:{projectId:$project,itemId:$item,fieldId:$field,value:{singleSelectOptionId:$option}}) { projectV2Item { id } }
        }""", {"project": project["id"], "item": item, "field": field["id"], "option": field["options"][matches[0]]})
    return {"configured": True, "item": item}
